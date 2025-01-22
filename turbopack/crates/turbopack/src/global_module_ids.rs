use anyhow::{Context, Result};
use tracing::Instrument;
use turbo_tasks::{FxIndexMap, FxIndexSet, ResolvedVc, TryJoinIterExt, ValueToString, Vc};
use turbo_tasks_hash::hash_xxh3_hash64;
use turbopack_core::{
    chunk::{module_id_strategies::GlobalModuleIdStrategy, ChunkableModule, ChunkingType},
    ident::AssetIdent,
    module::Module,
    module_graph::ModuleGraph,
};
use turbopack_ecmascript::async_chunk::module::AsyncLoaderModule;

#[turbo_tasks::function]
pub async fn get_global_module_id_strategy(
    module_graph: ResolvedVc<ModuleGraph>,
) -> Result<Vc<GlobalModuleIdStrategy>> {
    let span = tracing::info_span!("compute module id map");
    async move {
        let module_graph = module_graph.await?;
        let mut idents = module_graph
            .graphs
            .iter()
            .try_join()
            .await?
            .iter()
            .flat_map(|graph| graph.iter_nodes())
            .map(|m| m.module.ident().to_resolved())
            .collect::<Vec<_>>();

        // Additionally, add all the modules that are inserted by chunking (i.e. async loaders)
        module_graph
            .traverse_all_edges_unordered(|parent, current| {
                if let (_, &ChunkingType::Async) = parent {
                    let module =
                        ResolvedVc::try_sidecast_sync::<Box<dyn ChunkableModule>>(current.module)
                            .context("expected chunkable module for async reference")?;
                    idents.push(AsyncLoaderModule::asset_ident_for(*module).to_resolved());
                }
                Ok(())
            })
            .await?;

        let mut module_id_map = idents
            .into_iter()
            .map(|ident| async move {
                let ident = ident.await?;
                Ok((ident, hash_xxh3_hash64(&ident.to_string().await?)))
            })
            .try_join()
            .await?
            .into_iter()
            .collect::<FxIndexMap<_, _>>();

        merge_preprocessed_module_ids(&mut module_id_map);

        Ok(GlobalModuleIdStrategy { module_id_map }.cell())
    }
    .instrument(span)
    .await
}

const JS_MAX_SAFE_INTEGER: u64 = (1u64 << 53) - 1;

pub fn merge_preprocessed_module_ids(
    merged_module_ids: &mut FxIndexMap<ResolvedVc<AssetIdent>, u64>,
) {
    // 5% fill rate, as done in Webpack
    // https://github.com/webpack/webpack/blob/27cf3e59f5f289dfc4d76b7a1df2edbc4e651589/lib/ids/IdHelpers.js#L366-L405
    let optimal_range = merged_module_ids.len() * 20;
    let digit_mask = std::cmp::min(
        10u64.pow((optimal_range as f64).log10().ceil() as u32),
        JS_MAX_SAFE_INTEGER,
    );

    let mut used_ids = FxIndexSet::default();
    for full_hash in merged_module_ids.values_mut() {
        let mut trimmed_hash = *full_hash % digit_mask;
        let mut i = 0;
        while used_ids.contains(&trimmed_hash) {
            i += 1;
            // If the id is already used, seek to find another available id.
            trimmed_hash = hash_xxh3_hash64(*full_hash + i) % digit_mask;
        }
        used_ids.insert(trimmed_hash);
        *full_hash = trimmed_hash;
    }
}
