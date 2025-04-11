use std::{
    collections::{HashMap, VecDeque},
    hash::{DefaultHasher, Hasher},
    sync::{
        OnceLock, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    usize,
};

use crate::{SCOPE_DEPTH_MAX, SCOPE_STRING_SEP, Scope, ScopeAlloc, env_config};

use log as log_impl;

static ENV_FILTER: OnceLock<env_config::EnvFilter> = OnceLock::new();
static SCOPE_MAP: RwLock<Option<ScopeMap>> = RwLock::new(None);
static SCOPE_MAP_HASH: AtomicU64 = AtomicU64::new(0);

pub fn init_env_filter(filter: env_config::EnvFilter) {
    if ENV_FILTER.set(filter).is_err() {
        panic!("Environment filter cannot be initialized twice");
    }
}

pub fn is_scope_enabled(scope: &Scope, level: log_impl::Level) -> (bool, log_impl::Level) {
    let level_min = crate::min_printed_log_level(level);
    if level <= level_min {
        // [FAST PATH]
        // if the message is at or below the minimum printed log level
        // (where error < warn < info etc) then always enable
        return (true, level);
    }

    let Ok(map) = SCOPE_MAP.read() else {
        // on failure, default to enabled detection done by `log` crate
        return (true, level);
    };

    let Some(map) = map.as_ref() else {
        // on failure, default to enabled detection done by `log` crate
        return (true, level);
    };

    if map.is_empty() {
        // if no scopes are enabled, default to enabled detection done by `log` crate
        return (true, level);
    }
    let enabled_status = map.is_enabled(&scope, level);
    match enabled_status {
        EnabledStatus::NotConfigured => {
            // if this scope isn't configured, default to enabled detection done by `log` crate
            return (true, level);
        }
        EnabledStatus::Enabled => {
            // if this scope is enabled, enable logging
            // note: bumping level to min level that will be printed
            // to work around log crate limitations
            return (true, level_min);
        }
        EnabledStatus::Disabled => {
            // if the configured level is lower than the requested level, disable logging
            // note: err = 0, warn = 1, etc.
            return (false, level);
        }
    }
}

fn hash_scope_map_settings(map: &HashMap<String, String>) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut items = map.iter().collect::<Vec<_>>();
    items.sort();
    for (key, value) in items {
        Hasher::write(&mut hasher, key.as_bytes());
        Hasher::write(&mut hasher, value.as_bytes());
    }
    return hasher.finish();
}

pub(crate) fn refresh() {
    refresh_from_settings(&HashMap::default());
}

pub fn refresh_from_settings(settings: &HashMap<String, String>) {
    let hash_old = SCOPE_MAP_HASH.load(Ordering::Acquire);
    let hash_new = hash_scope_map_settings(settings);
    if hash_old == hash_new && hash_old != 0 {
        return;
    }
    let env_config = ENV_FILTER.get();
    let map_new = ScopeMap::new_from_settings_and_env(settings, env_config);

    if let Ok(_) =
        SCOPE_MAP_HASH.compare_exchange(hash_old, hash_new, Ordering::Release, Ordering::Relaxed)
    {
        let mut map = SCOPE_MAP.write().unwrap_or_else(|err| {
            SCOPE_MAP.clear_poison();
            err.into_inner()
        });
        *map = Some(map_new);
    }
}

fn level_from_level_str(level_str: &String) -> Option<log_impl::Level> {
    let level = match level_str.to_ascii_lowercase().as_str() {
        "" => log_impl::Level::Trace,
        "trace" => log_impl::Level::Trace,
        "debug" => log_impl::Level::Debug,
        "info" => log_impl::Level::Info,
        "warn" => log_impl::Level::Warn,
        "error" => log_impl::Level::Error,
        "off" | "disable" | "no" | "none" | "disabled" => {
            crate::warn!(
                "Invalid log level \"{level_str}\", set to error to disable non-error logging. Defaulting to error"
            );
            log_impl::Level::Error
        }
        _ => {
            crate::warn!("Invalid log level \"{level_str}\", ignoring");
            return None;
        }
    };
    return Some(level);
}

fn scope_alloc_from_scope_str(scope_str: &String) -> Option<ScopeAlloc> {
    let mut scope_buf = [""; SCOPE_DEPTH_MAX];
    let mut index = 0;
    let mut scope_iter = scope_str.split(SCOPE_STRING_SEP);
    while index < SCOPE_DEPTH_MAX {
        let Some(scope) = scope_iter.next() else {
            break;
        };
        if scope == "" {
            continue;
        }
        scope_buf[index] = scope;
        index += 1;
    }
    if index == 0 {
        return None;
    }
    if let Some(_) = scope_iter.next() {
        crate::warn!(
            "Invalid scope key, too many nested scopes: '{scope_str}'. Max depth is {SCOPE_DEPTH_MAX}",
        );
        return None;
    }
    let scope = scope_buf.map(|s| s.to_string());
    return Some(scope);
}

pub struct ScopeMap {
    entries: Vec<ScopeMapEntry>,
    root_count: usize,
}

pub struct ScopeMapEntry {
    scope: String,
    enabled: Option<log_impl::Level>,
    descendants: std::ops::Range<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnabledStatus {
    Enabled,
    Disabled,
    NotConfigured,
}

impl ScopeMap {
    pub fn new_from_settings_and_env(
        items_input_map: &HashMap<String, String>,
        env_config: Option<&env_config::EnvFilter>,
    ) -> Self {
        let mut items = Vec::with_capacity(
            items_input_map.len() + env_config.map_or(0, |c| c.directive_names.len()),
        );
        if let Some(env_filter) = env_config {
            // TODO: parse on load instead of every reload
            items.extend(
                env_filter
                    .directive_names
                    .iter()
                    .zip(env_filter.directive_levels.iter())
                    .filter_map(|(scope, level_filter)| {
                        if items_input_map.get(scope).is_some() {
                            return None;
                        }
                        let scope = scope_alloc_from_scope_str(scope)?;
                        // TODO: use level filters instead of scopes in scope map
                        let level = level_filter.to_level()?;

                        Some((scope, level))
                    }),
            );
        }
        items.extend(
            items_input_map
                .into_iter()
                .filter_map(|(scope_str, level_str)| {
                    let scope = scope_alloc_from_scope_str(&scope_str)?;
                    let level = level_from_level_str(&level_str)?;
                    return Some((scope, level));
                }),
        );

        items.sort_by(|a, b| a.0.cmp(&b.0));

        let mut this = Self {
            entries: Vec::with_capacity(items.len() * SCOPE_DEPTH_MAX),
            root_count: 0,
        };

        let items_count = items.len();

        struct ProcessQueueEntry {
            parent_index: usize,
            depth: usize,
            items_range: std::ops::Range<usize>,
        }
        let mut process_queue = VecDeque::new();
        process_queue.push_back(ProcessQueueEntry {
            parent_index: usize::MAX,
            depth: 0,
            items_range: 0..items_count,
        });

        let empty_range = 0..0;

        while let Some(process_entry) = process_queue.pop_front() {
            let ProcessQueueEntry {
                items_range,
                depth,
                parent_index,
            } = process_entry;
            let mut cursor = items_range.start;
            let res_entries_start = this.entries.len();
            while cursor < items_range.end {
                let sub_items_start = cursor;
                cursor += 1;
                let scope_name = &items[sub_items_start].0[depth];
                while cursor < items_range.end && &items[cursor].0[depth] == scope_name {
                    cursor += 1;
                }
                let sub_items_end = cursor;
                if scope_name == "" {
                    assert_eq!(sub_items_start + 1, sub_items_end);
                    assert_ne!(depth, 0);
                    assert_ne!(parent_index, usize::MAX);
                    assert!(this.entries[parent_index].enabled.is_none());
                    this.entries[parent_index].enabled = Some(items[sub_items_start].1);
                    continue;
                }
                let is_valid_scope = scope_name != "";
                let is_last = depth + 1 == SCOPE_DEPTH_MAX || !is_valid_scope;
                let mut enabled = None;
                if is_last {
                    assert_eq!(
                        sub_items_start + 1,
                        sub_items_end,
                        "Expected one item: got: {:?}",
                        &items[items_range.clone()]
                    );
                    enabled = Some(items[sub_items_start].1);
                } else {
                    let entry_index = this.entries.len();
                    process_queue.push_back(ProcessQueueEntry {
                        items_range: sub_items_start..sub_items_end,
                        parent_index: entry_index,
                        depth: depth + 1,
                    });
                }
                this.entries.push(ScopeMapEntry {
                    scope: scope_name.to_owned(),
                    enabled,
                    descendants: empty_range.clone(),
                });
            }
            let res_entries_end = this.entries.len();
            if parent_index != usize::MAX {
                this.entries[parent_index].descendants = res_entries_start..res_entries_end;
            } else {
                this.root_count = res_entries_end;
            }
        }

        return this;
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn is_enabled<S>(
        &self,
        scope: &[S; SCOPE_DEPTH_MAX],
        level: log_impl::Level,
    ) -> EnabledStatus
    where
        S: AsRef<str>,
    {
        let mut enabled = None;
        let mut cur_range = &self.entries[0..self.root_count];
        let mut depth = 0;

        'search: while !cur_range.is_empty()
            && depth < SCOPE_DEPTH_MAX
            && scope[depth].as_ref() != ""
        {
            for entry in cur_range {
                if entry.scope == scope[depth].as_ref() {
                    // note:
                    enabled = entry.enabled.or(enabled);
                    cur_range = &self.entries[entry.descendants.clone()];
                    depth += 1;
                    continue 'search;
                }
            }
            break 'search;
        }

        return enabled.map_or(EnabledStatus::NotConfigured, |level_enabled| {
            if level <= level_enabled {
                EnabledStatus::Enabled
            } else {
                EnabledStatus::Disabled
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use crate::private::scope_new;

    use super::*;

    fn scope_map_from_keys(kv: &[(&str, &str)]) -> ScopeMap {
        let hash_map: HashMap<String, String> = kv
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ScopeMap::new_from_settings_and_env(&hash_map, None)
    }

    #[test]
    fn test_initialization() {
        let map = scope_map_from_keys(&[("a.b.c.d", "trace")]);
        assert_eq!(map.root_count, 1);
        assert_eq!(map.entries.len(), 4);

        let map = scope_map_from_keys(&[]);
        assert_eq!(map.root_count, 0);
        assert_eq!(map.entries.len(), 0);

        let map = scope_map_from_keys(&[("", "trace")]);
        assert_eq!(map.root_count, 0);
        assert_eq!(map.entries.len(), 0);

        let map = scope_map_from_keys(&[("foo..bar", "trace")]);
        assert_eq!(map.root_count, 1);
        assert_eq!(map.entries.len(), 2);

        let map = scope_map_from_keys(&[
            ("a.b.c.d", "trace"),
            ("e.f.g.h", "debug"),
            ("i.j.k.l", "info"),
            ("m.n.o.p", "warn"),
            ("q.r.s.t", "error"),
        ]);
        assert_eq!(map.root_count, 5);
        assert_eq!(map.entries.len(), 20);
        assert_eq!(map.entries[0].scope, "a");
        assert_eq!(map.entries[1].scope, "e");
        assert_eq!(map.entries[2].scope, "i");
        assert_eq!(map.entries[3].scope, "m");
        assert_eq!(map.entries[4].scope, "q");
    }

    fn scope_from_scope_str(scope_str: &'static str) -> Scope {
        let mut scope_buf = [""; SCOPE_DEPTH_MAX];
        let mut index = 0;
        let mut scope_iter = scope_str.split(SCOPE_STRING_SEP);
        while index < SCOPE_DEPTH_MAX {
            let Some(scope) = scope_iter.next() else {
                break;
            };
            if scope == "" {
                continue;
            }
            scope_buf[index] = scope;
            index += 1;
        }
        assert_ne!(index, 0);
        assert!(scope_iter.next().is_none());
        return scope_buf;
    }

    #[test]
    fn test_is_enabled() {
        let map = scope_map_from_keys(&[
            ("a.b.c.d", "trace"),
            ("e.f.g.h", "debug"),
            ("i.j.k.l", "info"),
            ("m.n.o.p", "warn"),
            ("q.r.s.t", "error"),
        ]);
        use log_impl::Level;
        assert_eq!(
            map.is_enabled(&scope_from_scope_str("a.b.c.d"), Level::Trace),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_from_scope_str("a.b.c.d"), Level::Debug),
            EnabledStatus::Enabled
        );

        assert_eq!(
            map.is_enabled(&scope_from_scope_str("e.f.g.h"), Level::Debug),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_from_scope_str("e.f.g.h"), Level::Info),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_from_scope_str("e.f.g.h"), Level::Trace),
            EnabledStatus::Disabled
        );

        assert_eq!(
            map.is_enabled(&scope_from_scope_str("i.j.k.l"), Level::Info),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_from_scope_str("i.j.k.l"), Level::Warn),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_from_scope_str("i.j.k.l"), Level::Debug),
            EnabledStatus::Disabled
        );

        assert_eq!(
            map.is_enabled(&scope_from_scope_str("m.n.o.p"), Level::Warn),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_from_scope_str("m.n.o.p"), Level::Error),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_from_scope_str("m.n.o.p"), Level::Info),
            EnabledStatus::Disabled
        );

        assert_eq!(
            map.is_enabled(&scope_from_scope_str("q.r.s.t"), Level::Error),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_from_scope_str("q.r.s.t"), Level::Warn),
            EnabledStatus::Disabled
        );
    }

    fn scope_map_from_keys_and_env(kv: &[(&str, &str)], env: &env_config::EnvFilter) -> ScopeMap {
        let hash_map: HashMap<String, String> = kv
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ScopeMap::new_from_settings_and_env(&hash_map, Some(env))
    }

    #[test]
    fn test_initialization_with_env() {
        let env_filter = env_config::parse("a.b=debug,u=error").unwrap();
        let map = scope_map_from_keys_and_env(&[], &env_filter);
        assert_eq!(map.root_count, 2);
        assert_eq!(map.entries.len(), 3);
        assert_eq!(
            map.is_enabled(&scope_new(&["a"]), log_impl::Level::Debug),
            EnabledStatus::NotConfigured
        );
        assert_eq!(
            map.is_enabled(&scope_new(&["a", "b"]), log_impl::Level::Debug),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_new(&["a", "b", "c"]), log_impl::Level::Trace),
            EnabledStatus::Disabled
        );

        let env_filter = env_config::parse("a.b=debug,e.f.g.h=trace,u=error").unwrap();
        let map = scope_map_from_keys_and_env(
            &[
                ("a.b.c.d", "trace"),
                ("e.f.g.h", "debug"),
                ("i.j.k.l", "info"),
                ("m.n.o.p", "warn"),
                ("q.r.s.t", "error"),
            ],
            &env_filter,
        );
        assert_eq!(map.root_count, 6);
        assert_eq!(map.entries.len(), 21);
        assert_eq!(map.entries[0].scope, "a");
        assert_eq!(map.entries[1].scope, "e");
        assert_eq!(map.entries[2].scope, "i");
        assert_eq!(map.entries[3].scope, "m");
        assert_eq!(map.entries[4].scope, "q");
        assert_eq!(map.entries[5].scope, "u");
        assert_eq!(
            map.is_enabled(&scope_new(&["a", "b", "c", "d"]), log_impl::Level::Trace),
            EnabledStatus::Enabled
        );
        assert_eq!(
            map.is_enabled(&scope_new(&["a", "b", "c"]), log_impl::Level::Trace),
            EnabledStatus::Disabled
        );
        assert_eq!(
            map.is_enabled(&scope_new(&["u", "v"]), log_impl::Level::Warn),
            EnabledStatus::Disabled
        );
        // settings override env
        assert_eq!(
            map.is_enabled(&scope_new(&["e", "f", "g", "h"]), log_impl::Level::Trace),
            EnabledStatus::Disabled,
        );
    }
}
