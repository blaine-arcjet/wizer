//! Check that we get the same result whether we
//!
//! 1. Call the initialization function
//! 2. Call the main function
//!
//! or
//!
//! 1. Call the initialization function
//! 2. Snapshot with Wizer
//! 3. Instantiate the snapshot
//! 4. Call the instantiated snapshot's main function
//!
//! When checking that we get the same result, we don't just consider the main
//! function's results: we also consider memories and globals.

#![no_main]

use libfuzzer_sys::fuzz_target;
use wasmtime::*;

const FUEL: u32 = 1_000;

fuzz_target!(|module: wasm_smith::Module| {
    let _ = env_logger::try_init();

    let mut module = module;
    module.ensure_termination(FUEL).unwrap();
    let wasm = module.to_bytes();

    if log::log_enabled!(log::Level::Debug) {
        log::debug!("Writing test case to `test.wasm`");
        std::fs::write("test.wasm", &wasm).unwrap();
        if let Ok(wat) = wasmprinter::print_bytes(&wasm) {
            log::debug!("Writing disassembly to `test.wat`");
            std::fs::write("test.wat", wat).unwrap();
        }
    }

    let mut config = Config::new();
    config.cache_config_load_default().unwrap();
    config.wasm_multi_memory(true);
    config.wasm_multi_value(true);

    let engine = Engine::new(&config).unwrap();
    let module = Module::new(&engine, &wasm).unwrap();
    if module.imports().len() > 0 {
        // Not using the `WasmConfig` for this because we want to encourage
        // imports/exports between modules within the bundle, just not at the
        // top level.
        return;
    }

    let mut main_funcs = vec![];
    let mut init_funcs = vec![];
    for exp in module.exports() {
        if let ExternType::Func(ty) = exp.ty() {
            main_funcs.push(exp.name());
            if ty.params().len() == 0 && ty.results().len() == 0 {
                init_funcs.push(exp.name());
            }
        }
    }

    'init_loop: for init_func in init_funcs {
        log::debug!("Using initialization function: {:?}", init_func);

        // Create a wizened snapshot of the given Wasm using `init_func` as the
        // initialization routine.
        let mut wizer = wizer::Wizer::new();
        wizer
            .wasm_multi_memory(true)
            .wasm_multi_value(true)
            .init_func(init_func);
        let snapshot_wasm = match wizer.run(&wasm) {
            Err(_) => continue 'init_loop,
            Ok(s) => s,
        };
        let snapshot_module =
            Module::new(&engine, &snapshot_wasm).expect("snapshot should be valid wasm");

        // Now check that each "main" function behaves the same whether we call
        // it on an instantiated snapshot or if we instantiate the original
        // Wasm, call the initialization routine, and then call the "main"
        // function.
        'main_loop: for main_func in &main_funcs {
            if *main_func == init_func {
                // Wizer un-exports the initialization function, so we can't use
                // it as a main function.
                continue 'main_loop;
            }
            log::debug!("Using main function: {:?}", main_func);

            let mut store = Store::new(&engine, ());

            // Instantiate the snapshot and call the main function.
            let snapshot_instance = Instance::new(&mut store, &snapshot_module, &[]).unwrap();
            let snapshot_main_func = snapshot_instance.get_func(&mut store, main_func).unwrap();
            let main_args = wizer::dummy::dummy_values(snapshot_main_func.ty(&store).params());
            let mut snapshot_result =
                vec![wasmtime::Val::I32(0); snapshot_main_func.ty(&store).results().len()];
            let snapshot_call_result =
                snapshot_main_func.call(&mut store, &main_args, &mut snapshot_result);

            // Instantiate the original Wasm and then call the initialization
            // and main functions back to back.
            let instance = Instance::new(&mut store, &module, &[]).unwrap();
            let init_func = instance
                .get_typed_func::<(), ()>(&mut store, init_func)
                .unwrap();
            init_func.call(&mut store, ()).unwrap();
            let main_func = instance.get_func(&mut store, main_func).unwrap();
            let mut result = vec![wasmtime::Val::I32(0); main_func.ty(&store).results().len()];
            let call_result = main_func.call(&mut store, &main_args, &mut result);

            // Check that the function return values / traps are the same.
            match (snapshot_call_result, call_result) {
                // Both did not trap.
                (Ok(()), Ok(())) => {
                    assert_eq!(snapshot_result.len(), result.len());
                    for (s, r) in snapshot_result.iter().zip(result.iter()) {
                        assert_val_eq(s, r);
                    }
                }

                // Both trapped.
                (Err(_), Err(_)) => {}

                // Divergence.
                (s, r) => {
                    panic!(
                        "divergence between whether the main function traps or not!\n\n\
                         no snapshotting result = {:?}\n\n\
                         snapshotted result = {:?}",
                        r, s,
                    );
                }
            }

            // Assert that all other exports have the same state as well.
            let exports = snapshot_instance
                .exports(&mut store)
                .map(|export| export.name().to_string())
                .collect::<Vec<_>>();
            for name in exports.iter() {
                let export = snapshot_instance.get_export(&mut store, &name).unwrap();
                match export {
                    Extern::Global(snapshot_global) => {
                        let global = instance.get_global(&mut store, &name).unwrap();
                        assert_val_eq(&snapshot_global.get(&mut store), &global.get(&mut store));
                    }
                    Extern::Memory(snapshot_memory) => {
                        let memory = instance.get_memory(&mut store, &name).unwrap();
                        let snapshot_memory = snapshot_memory.data(&store);
                        let memory = memory.data(&store);
                        assert_eq!(snapshot_memory.len(), memory.len());
                        // NB: Don't use `assert_eq` here so that we don't
                        // try to print the full memories' debug
                        // representations on failure.
                        if snapshot_memory != memory {
                            panic!("divergence between snapshot and non-snapshot memories");
                        }
                    }
                    Extern::SharedMemory(_) | Extern::Func(_) | Extern::Table(_) => continue,
                }
            }
        }
    }
});

fn assert_val_eq(a: &Val, b: &Val) {
    match (a, b) {
        (Val::I32(a), Val::I32(b)) => assert_eq!(a, b),
        (Val::I64(a), Val::I64(b)) => assert_eq!(a, b),
        (Val::F32(a), Val::F32(b)) => assert!({
            let a = f32::from_bits(*a);
            let b = f32::from_bits(*b);
            a == b || (a.is_nan() && b.is_nan())
        }),
        (Val::F64(a), Val::F64(b)) => assert!({
            let a = f64::from_bits(*a);
            let b = f64::from_bits(*b);
            a == b || (a.is_nan() && b.is_nan())
        }),
        (Val::V128(a), Val::V128(b)) => assert_eq!(a, b),
        _ => panic!("{:?} != {:?}", a, b),
    }
}
