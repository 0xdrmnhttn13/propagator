//! Ownership extractor (`plan.md` §6 — `Owns` edges).
//!
//! Emits one `Owns` edge per procedure/function defined in a service's source
//! root. Cheap (pure config co-location, no parsing) and always-on. Excluded
//! from `get_impact` by default (`include_weak=false`) since ownership ≠ call.
//!
//! Tables are intentionally NOT owned (shared schema objects); only procs/funcs.

use crate::extract::RawDefKind;

/// Emit `Owns` edges from `service` to every proc/function def in `defs`.
pub fn owns_edges(service: &str, defs: &[crate::extract::RawDef]) -> Vec<crate::extract::RawEdge> {
    defs.iter()
        .filter(|d| matches!(d.kind, RawDefKind::Proc | RawDefKind::Function))
        .map(|d| {
            let to = match &d.schema {
                Some(s) => format!("{s}.{}", d.title),
                None => d.title.clone(),
            };
            crate::extract::RawEdge {
                kind: crate::model::EdgeKind::Owns,
                from: service.to_string(),
                to,
                path: d.path.clone(),
                line: d.line_start,
                from_kind: None,
            }
        })
        .collect()
}
