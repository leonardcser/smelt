use super::*;

impl TestApp {
    /// Side-channel: invoke the transactional `/reload` pipeline. It loads a
    /// fresh candidate, commits it only after validation, retires the previous
    /// generation, and fires `on_ready` with `ctx.kind = "reload"`. Named
    /// resources keep stable ids while anonymous resources from the retired
    /// generation are reaped.
    pub fn reload_lua(&mut self) {
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        self.app.reload_lua();
    }

    pub fn schedule_lua_reload(&mut self) -> bool {
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        self.app.schedule_lua_reload()
    }

    pub fn drain_idle_work(&mut self) -> bool {
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        self.app.drain_idle_work()
    }

    pub fn pending_lua_reload(&self) -> bool {
        self.app.pending_lua_reload
    }

    /// Run an arbitrary Lua snippet against the embedded runtime with
    /// the host pointer installed. Returns whether execution succeeded
    /// (a Lua-level error is *not* a fuzz failure - many generated
    /// snippets intentionally hit type errors that the bindings layer
    /// raises as mlua errors). Used by `lua_loop` to feed batched ops
    /// that reference each other via shared locals.
    pub fn run_lua(&mut self, snippet: &str) -> bool {
        let succeeded = {
            let _guard = crate::lua::install_app_ptr(&mut self.app);
            self.app.lua.lua.load(snippet).exec().is_ok()
        };
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        self.app.try_perform_scheduled_runtime_reconcile();
        succeeded
    }

    pub fn lua_int_global(&self, name: &str) -> Option<i64> {
        self.app.lua.lua.globals().get(name).ok()
    }

    /// Re-publish the signal diff + fire queued subscribers. Production
    /// runs this every main-loop tick (`app.rs:1068`); the harness
    /// skips that loop and exposes it here so tests can assert against
    /// the reactive `work_*` / `vim_mode` / `now` signals without driving
    /// a synthetic event.
    pub fn tick_signals(&mut self) {
        let _guard = crate::lua::install_app_ptr(&mut self.app);
        self.app.publish_diff_signals();
        self.app.drain_signals_pending();
    }

    /// Counts of bound names across the four reload-managed registries:
    /// `(bufs, wins, overlays, paints)`. Failed-reload checks snapshot
    /// these before and after `reload_lua()` and assert equality. Successful
    /// reloads may add declared resources and retire names omitted by the
    /// candidate.
    pub fn named_resource_counts(&self) -> (usize, usize, usize, usize) {
        let (bufs, wins, overlays) = self.app.ui.named_counts();
        (bufs, wins, overlays, self.app.paint_registry.named_count())
    }

    /// Enumerate every Lua function recorded by `LuaMod::fn_` at
    /// registration time. Returned tuples are `(module, name)`, e.g.
    /// `("smelt.buf", "new")`. The same registry powers
    /// `cargo xtask gen-lua-docs`, so any function visible in the
    /// reference docs is also fuzzable - and a freshly-added
    /// `smelt.foo.bar` flows into the fuzz target automatically, with
    /// no manual update to a hand-written `LuaOp` table.
    pub fn lua_doc_snapshot(&self) -> Vec<(&'static str, &'static str)> {
        smelt_core::lua::doc::snapshot()
            .into_iter()
            .map(|m| (m.module, m.name))
            .collect()
    }

    /// Force a full Lua GC, then walk every registered `LuaHandle`
    /// across the shared registries and assert it still resolves in the
    /// mlua registry. A `Value::Nil` after `gc_collect` means a Rust
    /// path dropped the handle's `RegistryKey` (or the key was wrong
    /// from the start) - the Rust→Lua reference is dangling. Used by
    /// `lua_loop` between batches so leaks surface attached to the op
    /// that caused them rather than at scenario teardown.
    pub fn assert_lua_handles_alive(&self) {
        let lua = &self.app.lua.lua;
        lua.gc_collect().expect("lua gc_collect failed");

        let check = |label: &str, handle: &smelt_core::lua::LuaHandle| {
            let val: mlua::Value = lua
                .registry_value(&handle.key)
                .unwrap_or_else(|e| panic!("FFI-LEDGER: registry_value({label}) failed: {e}"));
            if matches!(val, mlua::Value::Nil) {
                panic!("FFI-LEDGER: dangling handle in {label} (registry value is Nil after gc_collect)");
            }
        };

        let shared = self.app.lua.shared();
        let core = &shared.core;
        if let Ok(cbs) = core.callbacks.lock() {
            for (id, h) in cbs.iter() {
                check(&format!("callbacks[{id}]"), h);
            }
        }
        if let Ok(asks) = core.ask_callbacks.lock() {
            for (id, callbacks) in asks.iter() {
                if let Some(h) = &callbacks.response {
                    check(&format!("ask_callbacks[{id}].response"), h);
                }
                if let Some(h) = &callbacks.delta {
                    check(&format!("ask_callbacks[{id}].delta"), h);
                }
            }
        }
        if let Ok(cmds) = core.commands.lock() {
            for (name, cmd) in cmds.iter() {
                check(&format!("commands[{name}]"), &cmd.handle);
            }
        }
        if let Ok(kms) = core.keymaps.lock() {
            for (k, entry) in kms.iter() {
                check(&format!("keymaps[{k:?}]"), &entry.handle);
            }
        }
        if let Ok(tools) = core.tools.lock() {
            for (name, t) in tools.iter() {
                check(&format!("tools[{name}].execute"), &t.execute);
                if let Some(h) = &t.approval_patterns {
                    check(&format!("tools[{name}].approval_patterns"), h);
                }
                if let Some(h) = &t.preflight {
                    check(&format!("tools[{name}].preflight"), h);
                }
                if let Some(h) = &t.paths_for_workspace {
                    check(&format!("tools[{name}].paths_for_workspace"), h);
                }
                if let Some(h) = &t.preview {
                    check(&format!("tools[{name}].preview"), h);
                }
                if let Some(h) = &t.preview_output {
                    check(&format!("tools[{name}].preview_output"), h);
                }
            }
        }

        let hooks = &core.hooks;
        let check_reg = |reg_label: &str, reg: &smelt_core::lua::HookRegistry| {
            reg.for_each_entry(|id, name, h| {
                check(&format!("{reg_label}[{id} name={name:?}]"), h);
            });
        };
        check_reg("hooks.tool_before", &hooks.tool_before);
        check_reg("hooks.tool_after", &hooks.tool_after);
        check_reg("hooks.provider_response", &hooks.provider_response);
        check_reg("hooks.context_limit", &hooks.context_limit);
        check_reg("hooks.lifecycle", &hooks.lifecycle);
    }

    /// Net live `LuaHandle` count, taken from this session's drop-counter
    /// ledger (`created - dropped`). Complements [`assert_lua_handles_alive`]:
    /// that function walks named registries and asserts each handle
    /// resolves; this one counts *every* handle that's ever crossed
    /// `LuaHandle::from_func` regardless of where it ended up stored,
    /// so it catches leaks the named walk can't see (anonymous closures
    /// stashed only in Lua tables, etc.).
    pub fn lua_handles_live(&self) -> u64 {
        self.app.lua.core_shared().lua_handles_live()
    }

    /// Reload the Lua context once, snapshot the live handle count,
    /// reload again, and assert the count didn't grow. Compares **two
    /// consecutive** reloads (not pre/post a single reload) because
    /// cold-start vs first-reload isn't apples-to-apples - lifecycle
    /// hooks fire with `ctx.kind = "reload"` only on the second-and-
    /// later bring-ups, so plugins legitimately do extra registration
    /// on the first reload. By reload N the system is in steady state;
    /// any drift between reload N and N+1 means a registry isn't
    /// being cleared.
    ///
    /// Intended for one-shot use at the END of a scenario, after the
    /// scenario's own reload ops have run - calling it inside the
    /// segment loop would inflate the reload count and obscure the
    /// scenario semantics.
    pub fn assert_no_handle_leak_across_reload(&mut self) {
        self.reload_lua();
        self.app.lua.lua.gc_collect().ok();
        self.app.lua.lua.gc_collect().ok();
        let baseline = self.app.lua.core_shared().lua_handles_live();
        self.reload_lua();
        self.app.lua.lua.gc_collect().ok();
        self.app.lua.lua.gc_collect().ok();
        let after = self.app.lua.core_shared().lua_handles_live();
        if after > baseline {
            panic!(
                "FFI-LEDGER: handle leak across reload - count went {baseline} -> {after} on second consecutive reload (steady state should be stable)"
            );
        }
    }

    /// Repeatedly load a candidate that allocates handles and then fails. The
    /// committed generation and named resources must remain unchanged, and
    /// discarding each candidate must return the session handle ledger to its
    /// baseline.
    pub fn assert_no_handle_leak_across_failed_reload(&mut self, init_path: &std::path::Path) {
        let original = std::fs::read(init_path).unwrap_or_default();
        let generation = self.app.core.lua_generation;
        let named_resources = self.named_resource_counts();
        self.app.lua.lua.gc_collect().ok();
        self.app.lua.lua.gc_collect().ok();
        let baseline = self.app.lua.core_shared().lua_handles_live();
        let failing_candidate = r#"
            local buffer = smelt.buf.new({ name = "fuzz.failed.buffer" })
            buffer:source("discarded")
            smelt.cmd.register("fuzz.failed", function() end)
            smelt.signal.new("fuzz.failed.signal", 1)
            smelt.signal.subscribe("fuzz.failed.signal", function() end)
            smelt.timer.set(60000, function() end)
            smelt.lifecycle.on_ready(function() end)
            error("intentional failed candidate")
        "#;

        for _ in 0..2 {
            std::fs::write(init_path, failing_candidate).expect("write failing Lua candidate");
            self.reload_lua();
            self.app.lua.lua.gc_collect().ok();
            self.app.lua.lua.gc_collect().ok();
            assert_eq!(self.app.core.lua_generation, generation);
            assert_eq!(self.named_resource_counts(), named_resources);
            let after = self.app.lua.core_shared().lua_handles_live();
            if after > baseline {
                panic!(
                    "FFI-LEDGER: handle leak across failed reload - count went {baseline} -> {after}"
                );
            }
            self.assert_lua_handles_alive();
            self.assert_invariants();
        }

        std::fs::write(init_path, original).expect("restore Lua init after failed candidate");
    }
}
