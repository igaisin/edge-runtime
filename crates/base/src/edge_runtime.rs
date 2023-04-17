use crate::utils::units::{bytes_to_display, human_elapsed, mib_to_bytes};

use crate::js_worker::module_loader;
use crate::js_worker::types;
use anyhow::{anyhow, bail, Error};
use deno_core::located_script_name;
use deno_core::url::Url;
use deno_core::JsRuntime;
use deno_core::ModuleSpecifier;
use deno_core::RuntimeOptions;
use import_map::{parse_from_json, ImportMap, ImportMapDiagnostic};
use log::{debug, error, warn};
use std::collections::HashMap;
use std::fs;
use std::panic;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::thread;
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

use crate::snapshot;
use module_loader::DefaultModuleLoader;
use sb_core::http_start::sb_core_http;
use sb_core::net::sb_core_net;
use sb_core::permissions::{sb_core_permissions, Permissions};
use sb_core::runtime::sb_core_runtime;
use sb_core::sb_core_main_js;
use sb_env::sb_env as sb_env_op;
use sb_worker_context::essentials::{
    EdgeContextInitOpts, EdgeContextOpts, EdgeUserRuntimeOpts, UserWorkerMsgs,
};
use sb_workers::sb_user_workers;

fn load_import_map(maybe_path: Option<String>) -> Result<Option<ImportMap>, Error> {
    if let Some(path_str) = maybe_path {
        let path = Path::new(&path_str);
        let json_str = fs::read_to_string(path)?;

        let abs_path = std::env::current_dir().map(|p| p.join(path))?;
        let base_url = Url::from_directory_path(abs_path.parent().unwrap()).unwrap();
        let result = parse_from_json(&base_url, json_str.as_str())?;
        print_import_map_diagnostics(&result.diagnostics);
        Ok(Some(result.import_map))
    } else {
        Ok(None)
    }
}

fn print_import_map_diagnostics(diagnostics: &[ImportMapDiagnostic]) {
    if !diagnostics.is_empty() {
        warn!(
            "Import map diagnostics:\n{}",
            diagnostics
                .iter()
                .map(|d| format!("  - {d}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
}

pub struct EdgeRuntime {
    pub js_runtime: JsRuntime,
    pub main_module_url: ModuleSpecifier,
    pub is_user_runtime: bool,
    pub env_vars: HashMap<String, String>,
    pub conf: EdgeContextOpts,
    pub curr_user_opts: EdgeUserRuntimeOpts,
}

impl EdgeRuntime {
    pub fn new(opts: EdgeContextInitOpts) -> Result<Self, Error> {
        let EdgeContextInitOpts {
            service_path,
            no_module_cache,
            import_map_path,
            env_vars,
            conf,
        } = opts;

        let (is_user_runtime, user_rt_opts) = match conf.clone() {
            EdgeContextOpts::UserWorker(conf) => (true, conf.clone()),
            EdgeContextOpts::MainWorker(conf) => (false, EdgeUserRuntimeOpts::default()),
        };

        let user_agent = "supabase-edge-runtime".to_string();
        let base_url =
            Url::from_directory_path(std::env::current_dir().map(|p| p.join(&service_path))?)
                .unwrap();
        // TODO: check for other potential main paths (eg: index.js, index.tsx)
        let main_module_url = base_url.join("index.ts")?;

        // Note: this will load Mozilla's CAs (we may also need to support system certs)
        let root_cert_store = deno_tls::create_default_root_cert_store();

        let extensions = vec![
            sb_core_permissions::init_ops(),
            deno_webidl::deno_webidl::init_ops(),
            deno_console::deno_console::init_ops(),
            deno_url::deno_url::init_ops(),
            deno_web::deno_web::init_ops::<Permissions>(deno_web::BlobStore::default(), None),
            deno_fetch::deno_fetch::init_ops::<Permissions>(deno_fetch::Options {
                user_agent: user_agent.clone(),
                root_cert_store: Some(root_cert_store.clone()),
                ..Default::default()
            }),
            deno_websocket::deno_websocket::init_ops::<Permissions>(
                user_agent,
                Some(root_cert_store.clone()),
                None,
            ),
            // TODO: support providing a custom seed for crypto
            deno_crypto::deno_crypto::init_ops(None),
            deno_net::deno_net::init_ops::<Permissions>(Some(root_cert_store), false, None),
            deno_tls::deno_tls::init_ops(),
            deno_http::deno_http::init_ops(),
            sb_env_op::init_ops(),
            sb_user_workers::init_ops(),
            sb_core_main_js::init_ops(),
            sb_core_net::init_ops(),
            sb_core_http::init_ops(),
            sb_core_runtime::init_ops(Some(main_module_url.clone())),
        ];

        let import_map = load_import_map(import_map_path)?;
        let module_loader = DefaultModuleLoader::new(import_map, no_module_cache)?;

        let js_runtime = JsRuntime::new(RuntimeOptions {
            extensions,
            module_loader: Some(Rc::new(module_loader)),
            is_main: true,
            create_params: {
                if is_user_runtime {
                    Some(deno_core::v8::CreateParams::default().heap_limits(
                        mib_to_bytes(1) as usize,
                        mib_to_bytes(user_rt_opts.memory_limit_mb) as usize,
                    ))
                } else {
                    None
                }
            },
            shared_array_buffer_store: None,
            compiled_wasm_module_store: None,
            startup_snapshot: Some(snapshot::snapshot()),
            ..Default::default()
        });

        Ok(Self {
            js_runtime,
            main_module_url,
            is_user_runtime,
            env_vars,
            conf,
            curr_user_opts: user_rt_opts,
        })
    }

    pub async fn run(
        mut self,
        stream: UnixStream,
        shutdown_tx: oneshot::Sender<()>,
    ) -> Result<(), Error> {
        let is_user_rt = self.is_user_runtime;

        // Bootstrapping stage
        let script = format!(
            "globalThis.bootstrapSBEdge({}, {})",
            deno_core::serde_json::json!({ "target": env!("TARGET") }),
            is_user_rt
        );

        self.js_runtime
            .execute_script::<String>(located_script_name!(), script)
            .expect("Failed to execute bootstrap script");

        let (unix_stream_tx, unix_stream_rx) = mpsc::unbounded_channel::<UnixStream>();
        if let Err(e) = unix_stream_tx.send(stream) {
            return bail!(e);
        }

        //run inside a closure, so op_state_rc is released
        let env_vars = self.env_vars.clone();
        {
            let op_state_rc = self.js_runtime.op_state();
            let mut op_state = op_state_rc.borrow_mut();
            op_state.put::<mpsc::UnboundedReceiver<UnixStream>>(unix_stream_rx);
            op_state.put::<sb_env::EnvVars>(env_vars);

            if !is_user_rt {
                if let EdgeContextOpts::MainWorker(conf) = self.conf.clone() {
                    op_state
                        .put::<mpsc::UnboundedSender<UserWorkerMsgs>>(conf.worker_pool_tx.clone());
                }
            }
        }

        let (halt_isolate_tx, mut halt_isolate_rx) = oneshot::channel::<()>();

        if is_user_rt {
            let (memory_limit_tx, memory_limit_rx) = mpsc::unbounded_channel::<u64>();

            // add a callback when a worker reaches its memory limit
            let memory_limit_mb = self.curr_user_opts.memory_limit_mb;
            self.js_runtime
                .add_near_heap_limit_callback(move |cur, _init| {
                    debug!(
                        "[{}] Low memory alert triggered: {}",
                        "x",
                        bytes_to_display(cur as u64),
                    );
                    let _ = memory_limit_tx.send(mib_to_bytes(memory_limit_mb));
                    // add a 25% allowance to memory limit
                    let cur =
                        mib_to_bytes(memory_limit_mb + memory_limit_mb.div_euclid(4)) as usize;
                    cur
                });

            self.start_controller_thread(
                self.curr_user_opts.worker_timeout_ms,
                memory_limit_rx,
                halt_isolate_tx,
            );
        }

        let mut js_runtime = self.js_runtime;

        let future = async move {
            let mod_id = js_runtime
                .load_main_module(&self.main_module_url, None)
                .await?;
            let mod_result = js_runtime.mod_evaluate(mod_id);

            let result = tokio::select! {
                _ = js_runtime.run_event_loop(false) => {
                    debug!("Event loop has completed");
                    mod_result.await?
                },
                _ = &mut halt_isolate_rx => {
                    debug!("User Worker execution halted");
                    Ok(())
                }
            };

            drop(js_runtime);
            result
        };

        let res = future.await;

        if res.is_err() {
            error!("worker thread panicked {:?}", res.as_ref().err().unwrap());
        }

        shutdown_tx.send(()).unwrap();
        Ok(())
    }

    fn start_controller_thread(
        &mut self,
        worker_timeout_ms: u64,
        mut memory_limit_rx: mpsc::UnboundedReceiver<u64>,
        halt_execution_tx: oneshot::Sender<()>,
    ) {
        let thread_safe_handle = self.js_runtime.v8_isolate().thread_safe_handle();

        thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let future = async move {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(worker_timeout_ms)) => {
                        debug!("max duration reached for the worker. terminating the worker. (duration {})", human_elapsed(worker_timeout_ms))
                    }
                    Some(val) = memory_limit_rx.recv() => {
                        error!("memory limit reached for the worker. terminating the worker. (used: {})", bytes_to_display(val));
                        thread_safe_handle.terminate_execution();
                    }
                }
            };
            rt.block_on(future);

            if halt_execution_tx.send(()).is_err() {
                error!("failed to send the halt execution signal");
            }
        });
    }
}
