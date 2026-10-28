use std::{fs, io};

use anyhow::{Context as _, Result};
use rquickjs::{Context as JsContext, Function, Module, Object, Persistent, Value};
use rquickjs_jit::JitRuntime as Runtime;
use tracing::debug;

/// The user's optional JavaScript configuration.
///
/// Field order is deliberate: persistent JS handles must be dropped before the
/// context and runtime they belong to.
pub struct Config {
    expand: Option<Persistent<Function<'static>>>,
    context: Option<JsContext>,
    _runtime: Option<Runtime>,
}

impl Config {
    /// Load `~/.config/wayime/config.js`. A missing file means no JS overrides.
    pub fn load_default() -> Result<Self> {
        let config_dir = dirs::config_dir().context("could not determine user config directory")?;
        let path = config_dir.join("wayime/config.js");
        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                debug!(path = %path.display(), "no JavaScript config found");
                return Ok(Self::empty());
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to read {}", path.display()));
            }
        };

        let runtime = Runtime::builder()
            .build()
            .context("failed to create QuickJS runtime")?;
        let context = JsContext::full(&runtime).context("failed to create QuickJS context")?;
        let expand = context
            .with(|ctx| -> rquickjs::Result<Persistent<Function<'static>>> {
                let module = Module::declare(ctx.clone(), "wayime-config.js", source)?;
                let (module, evaluation) = module.eval()?;
                evaluation.finish::<()>()?;

                let config = module.namespace()?.get::<_, Object>("default")?;
                // Validate the public contract eagerly, so a typo fails at startup.
                let _: Function = config.get("expand")?;

                // Preserve method-call semantics (`this` refers to the default object)
                // while keeping a callable handle for repeated synchronous lookups.
                let factory =
                    ctx.eval::<Function, _>("(config) => (code) => config.expand(code)")?;
                let expand = factory.call::<_, Function>((config,))?;
                Ok(Persistent::save(&ctx, expand))
            })
            .with_context(|| format!("failed to evaluate {}", path.display()))?;

        debug!(path = %path.display(), "JavaScript config loaded");
        Ok(Self {
            expand: Some(expand),
            context: Some(context),
            _runtime: Some(runtime),
        })
    }

    fn empty() -> Self {
        Self {
            expand: None,
            context: None,
            _runtime: None,
        }
    }

    /// Return a replacement candidate, or `None` to use the dictionary.
    /// JavaScript exceptions and non-string return values are reported to the caller.
    pub fn expand(&self, code: &str) -> Result<Option<String>> {
        let (Some(context), Some(expand)) = (&self.context, &self.expand) else {
            return Ok(None);
        };

        context
            .with(|ctx| -> rquickjs::Result<Option<String>> {
                let expand = expand.clone().restore(&ctx)?;
                let value: Value = expand.call((code,))?;
                if value.is_undefined() {
                    Ok(None)
                } else {
                    Ok(Some(value.get::<String>()?))
                }
            })
            .map_err(Into::into)
    }
}
