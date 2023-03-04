// Copyright (C) 2022 Nydus Developers. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::convert::TryFrom;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use hex::FromHex;
use nydus_api::ConfigV2;
use nydus_rafs::builder::{
    ArtifactStorage, BlobContext, BlobManager, Bootstrap, BootstrapContext, BuildContext,
    BuildOutput, ChunkSource, HashChunkDict, MetadataTreeBuilder, Overlay, Tree, WhiteoutSpec,
};
use nydus_rafs::metadata::{RafsInodeExt, RafsSuper, RafsVersion};
use nydus_storage::device::{BlobFeatures, BlobInfo};

/// Struct to generate the merged RAFS bootstrap for an image from per layer RAFS bootstraps.
///
/// A container image contains one or more layers, a RAFS bootstrap is built for each layer.
/// Those per layer bootstraps could be mounted by overlayfs to form the container rootfs.
/// To improve performance by avoiding overlayfs, an image level bootstrap is generated by
/// merging per layer bootstrap with overlayfs rules applied.
pub struct Merger {}

impl Merger {
    fn get_digest_from_list(digests: &Option<Vec<String>>, idx: usize) -> Result<Option<[u8; 32]>> {
        Ok(if let Some(digests) = &digests {
            let digest = digests
                .get(idx)
                .ok_or_else(|| anyhow!("unmatched digest index {}", idx))?;
            Some(<[u8; 32]>::from_hex(digest)?)
        } else {
            None
        })
    }

    fn get_size_from_list(sizes: &Option<Vec<u64>>, idx: usize) -> Result<Option<u64>> {
        Ok(if let Some(sizes) = &sizes {
            let size = sizes
                .get(idx)
                .ok_or_else(|| anyhow!("unmatched size index {}", idx))?;
            Some(*size)
        } else {
            None
        })
    }

    /// Generate the merged RAFS bootstrap for an image from per layer RAFS bootstraps.
    ///
    /// # Arguments
    /// - sources: contains one or more per layer bootstraps in order of lower to higher.
    /// - chunk_dict: contain the chunk dictionary used to build per layer boostrap, or None.
    #[allow(clippy::too_many_arguments)]
    pub fn merge(
        ctx: &mut BuildContext,
        sources: Vec<PathBuf>,
        blob_digests: Option<Vec<String>>,
        blob_sizes: Option<Vec<u64>>,
        blob_toc_digests: Option<Vec<String>>,
        blob_toc_sizes: Option<Vec<u64>>,
        target: ArtifactStorage,
        chunk_dict: Option<PathBuf>,
        config_v2: Arc<ConfigV2>,
    ) -> Result<BuildOutput> {
        if sources.is_empty() {
            bail!("source bootstrap list is empty , at least one bootstrap is required");
        }
        if let Some(digests) = blob_digests.as_ref() {
            ensure!(
                digests.len() == sources.len(),
                "number of blob digest entries {} doesn't match number of sources {}",
                digests.len(),
                sources.len(),
            );
        }
        if let Some(toc_digests) = blob_toc_digests.as_ref() {
            ensure!(
                toc_digests.len() == sources.len(),
                "number of toc digest entries {} doesn't match number of sources {}",
                toc_digests.len(),
                sources.len(),
            );
        }
        if let Some(sizes) = blob_sizes.as_ref() {
            ensure!(
                sizes.len() == sources.len(),
                "number of blob size entries {} doesn't match number of sources {}",
                sizes.len(),
                sources.len(),
            );
        }
        if let Some(sizes) = blob_toc_sizes.as_ref() {
            ensure!(
                sizes.len() == sources.len(),
                "number of toc size entries {} doesn't match number of sources {}",
                sizes.len(),
                sources.len(),
            );
        }

        // Get the blobs come from chunk dict bootstrap.
        let mut chunk_dict_blobs = HashSet::new();
        let mut config = None;
        if let Some(chunk_dict_path) = &chunk_dict {
            let (rs, _) =
                RafsSuper::load_from_file(chunk_dict_path, config_v2.clone(), true, false)
                    .context(format!("load chunk dict bootstrap {:?}", chunk_dict_path))?;
            config = Some(rs.meta.get_config());
            for blob in rs.superblock.get_blob_infos() {
                chunk_dict_blobs.insert(blob.blob_id().to_string());
            }
        }

        let mut fs_version = RafsVersion::V6;
        let mut chunk_size = None;
        let mut tree: Option<Tree> = None;
        let mut blob_mgr = BlobManager::new(ctx.digester);

        for (layer_idx, bootstrap_path) in sources.iter().enumerate() {
            let (rs, _) = RafsSuper::load_from_file(bootstrap_path, config_v2.clone(), true, false)
                .context(format!("load bootstrap {:?}", bootstrap_path))?;
            config
                .get_or_insert_with(|| rs.meta.get_config())
                .check_compatibility(&rs.meta)?;
            fs_version = RafsVersion::try_from(rs.meta.version)
                .context("failed to get RAFS version number")?;
            ctx.compressor = rs.meta.get_compressor();
            ctx.digester = rs.meta.get_digester();
            ctx.explicit_uidgid = rs.meta.explicit_uidgid();

            let mut blob_idx_map = Vec::new();
            let mut parent_blob_added = false;
            for blob in rs.superblock.get_blob_infos() {
                let mut blob_ctx = BlobContext::from(ctx, &blob, ChunkSource::Parent)?;
                if let Some(chunk_size) = chunk_size {
                    ensure!(
                        chunk_size == blob_ctx.chunk_size,
                        "can not merge bootstraps with inconsistent chunk size, current bootstrap {:?} with chunk size {:x}, expected {:x}",
                        bootstrap_path,
                        blob_ctx.chunk_size,
                        chunk_size,
                    );
                } else {
                    chunk_size = Some(blob_ctx.chunk_size);
                }
                if chunk_dict_blobs.get(&blob.blob_id()).is_none() {
                    // It is assumed that the `nydus-image create` at each layer and `nydus-image merge` commands
                    // use the same chunk dict bootstrap. So the parent bootstrap includes multiple blobs, but
                    // only at most one new blob, the other blobs should be from the chunk dict image.
                    if parent_blob_added {
                        bail!("invalid per layer bootstrap, having multiple associated data blobs");
                    }
                    parent_blob_added = true;

                    if ctx.configuration.internal.blob_accessible() {
                        // `blob.blob_id()` should have been fixed when loading the bootstrap.
                        blob_ctx.blob_id = blob.blob_id();
                    } else {
                        // The blob id (blob sha256 hash) in parent bootstrap is invalid for nydusd
                        // runtime, should change it to the hash of whole tar blob.
                        blob_ctx.blob_id = BlobInfo::get_blob_id_from_meta_path(bootstrap_path)?;
                    }
                    if let Some(digest) = Self::get_digest_from_list(&blob_digests, layer_idx)? {
                        if blob.has_feature(BlobFeatures::SEPARATE) {
                            blob_ctx.blob_meta_digest = digest;
                        } else {
                            blob_ctx.blob_id = hex::encode(digest);
                        }
                    }
                    if let Some(size) = Self::get_size_from_list(&blob_sizes, layer_idx)? {
                        if blob.has_feature(BlobFeatures::SEPARATE) {
                            blob_ctx.blob_meta_size = size;
                        } else {
                            blob_ctx.compressed_blob_size = size;
                        }
                    }
                    if let Some(digest) = Self::get_digest_from_list(&blob_toc_digests, layer_idx)?
                    {
                        blob_ctx.blob_toc_digest = digest;
                    }
                    if let Some(size) = Self::get_size_from_list(&blob_toc_sizes, layer_idx)? {
                        blob_ctx.blob_toc_size = size as u32;
                    }
                }

                let mut found = false;
                for (idx, blob) in blob_mgr.get_blobs().iter().enumerate() {
                    if blob.blob_id == blob_ctx.blob_id {
                        blob_idx_map.push(idx as u32);
                        found = true;
                    }
                }
                if !found {
                    blob_idx_map.push(blob_mgr.len() as u32);
                    blob_mgr.add_blob(blob_ctx);
                }
            }

            if let Some(tree) = &mut tree {
                let mut nodes = Vec::new();
                rs.walk_directory::<PathBuf>(
                    rs.superblock.root_ino(),
                    None,
                    &mut |inode: Arc<dyn RafsInodeExt>, path: &Path| -> Result<()> {
                        let mut node =
                            MetadataTreeBuilder::parse_node(&rs, inode, path.to_path_buf())
                                .context(format!(
                                    "parse node from bootstrap {:?}",
                                    bootstrap_path
                                ))?;
                        for chunk in &mut node.chunks {
                            let origin_blob_index = chunk.inner.blob_index() as usize;
                            // Set the blob index of chunk to real index in blob table of final bootstrap.
                            chunk.set_blob_index(blob_idx_map[origin_blob_index]);
                        }
                        // Set node's layer index to distinguish same inode number (from bootstrap)
                        // between different layers.
                        node.layer_idx = u16::try_from(layer_idx).context(format!(
                            "too many layers {}, limited to {}",
                            layer_idx,
                            u16::MAX
                        ))?;
                        node.overlay = Overlay::UpperAddition;
                        match node.whiteout_type(WhiteoutSpec::Oci) {
                            // Insert whiteouts at the head, so they will be handled first when
                            // applying to lower layer.
                            Some(_) => nodes.insert(0, node),
                            _ => nodes.push(node),
                        }
                        Ok(())
                    },
                )?;
                for node in &nodes {
                    tree.apply(node, true, WhiteoutSpec::Oci)?;
                }
            } else {
                let mut dict = HashChunkDict::new(rs.meta.get_digester());
                tree = Some(Tree::from_bootstrap(&rs, &mut dict)?);
            }
        }

        // Safe to unwrap because there is at least one source bootstrap.
        let tree = tree.unwrap();
        ctx.fs_version = fs_version;
        if let Some(chunk_size) = chunk_size {
            ctx.chunk_size = chunk_size;
        }

        let mut bootstrap_ctx = BootstrapContext::new(Some(target.clone()), false)?;
        let mut bootstrap = Bootstrap::new()?;
        bootstrap.build(ctx, &mut bootstrap_ctx, tree)?;
        let blob_table = blob_mgr.to_blob_table(ctx)?;
        let mut bootstrap_storage = Some(target.clone());
        bootstrap
            .dump(ctx, &mut bootstrap_storage, &mut bootstrap_ctx, &blob_table)
            .context(format!("dump bootstrap to {:?}", target.display()))?;
        BuildOutput::new(&blob_mgr, &bootstrap_storage)
    }
}
