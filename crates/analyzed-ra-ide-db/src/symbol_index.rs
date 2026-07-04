use std::ops::ControlFlow;

use base_db::{Crate as BaseCrate, LibraryRoots, LocalRoots, source_root_crates};
use rayon::prelude::*;

use super::{Query, SymbolIndex, crate_symbols};
use crate::RootDatabase;

pub fn world_symbols(db: &RootDatabase, mut query: Query) -> Vec<hir::symbols::FileSymbol<'_>> {
    let _p = tracing::info_span!("world_symbols", query = ?query.query).entered();
    let indices = if query.is_crate_search() {
        query.only_types = false;
        vec![SymbolIndex::extern_prelude_symbols(db)]
    } else if !query.path_filter.is_empty() {
        query.only_types = false;
        let modules = crate::analyzed::resolve_path_to_modules(
            db,
            &query.path_filter,
            query.anchor_to_crate,
            query.case_sensitive,
        );
        if modules.is_empty() {
            return Vec::new();
        }
        modules.into_iter().map(|module| SymbolIndex::module_symbols(db, module)).collect()
    } else if query.libs {
        library_indices(db)
    } else {
        local_indices(db)
    };

    let mut symbols = Vec::new();
    query.search::<()>(db, &indices, |symbol| {
        if crate::analyzed::is_symbol_visible(db, symbol) {
            symbols.push(symbol.clone());
        }
        ControlFlow::Continue(())
    });
    symbols
}

fn library_indices(db: &RootDatabase) -> Vec<&SymbolIndex<'_>> {
    let roots = LibraryRoots::get(db).roots(db);
    roots.par_iter()
        .for_each_with(db.clone(), |snap, &root| _ = SymbolIndex::library_symbols(snap, root));
    roots.iter().map(|&root| SymbolIndex::library_symbols(db, root)).collect()
}

fn local_indices(db: &RootDatabase) -> Vec<&SymbolIndex<'_>> {
    let crates = visible_local_crates(db);
    crates.par_iter().for_each_with(db.clone(), |snap, &krate| {
        _ = crate_symbols(snap, krate.into());
    });
    crates
        .into_iter()
        .flat_map(|krate| Vec::from(crate_symbols(db, krate.into())))
        .chain(std::iter::once(SymbolIndex::extern_prelude_symbols(db)))
        .collect()
}

fn visible_local_crates(db: &RootDatabase) -> Vec<BaseCrate> {
    LocalRoots::get(db)
        .roots(db)
        .iter()
        .flat_map(|&root| source_root_crates(db, root).iter().copied())
        .filter(|&krate| db.is_crate_visible(krate))
        .collect()
}
