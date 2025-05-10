use std::cell::{ OnceCell, RefCell };
use std::io;
use std::sync::{ Mutex, OnceLock };
use std::task::{ Context, Poll };
use crossterm::terminal::WindowSize;
use futures::StreamExt;
use futures::channel::mpsc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::Closure;
use web_sys::HtmlElement;
use xterm_js_rs::addons::fit::FitAddon;
use xterm_js_rs::addons::webgl::WebglAddon;

thread_local! {
    static TERMINAL: OnceCell<xterm_js_rs::Terminal> = const { OnceCell::new() };
    static FIT_ADDON: OnceCell<Option<wasm_bindgen::JsValue>> = const { OnceCell::new() };
    static RESIZE_CALLBACK: OnceCell<Closure<dyn FnMut()>> = const { OnceCell::new() };
    static USE_FIT: OnceCell<bool> = const { OnceCell::new() };
}

static DATA_CHANNEL: OnceLock<Mutex<mpsc::Receiver<String>>> = OnceLock::new();

/// Terminal configuration options
pub struct TerminalConfig {
    /// Whether to use the FitAddon
    pub use_fit: bool,
    /// Whether to use the WebglAddon
    pub use_webgl: bool,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            use_fit: true,
            use_webgl: true,
        }
    }
}

pub(crate) fn with_terminal<F, T>(f: F) -> T where F: FnOnce(&xterm_js_rs::Terminal) -> T {
    TERMINAL.with(|t| f(t.get().unwrap()))
}

pub(crate) fn with_fit_addon<F, T>(f: F) -> Option<T> where F: FnOnce(&FitAddon) -> T {
    FIT_ADDON.with(|addon| {
        addon
            .get()
            .unwrap()
            .as_ref()
            .map(|fit_addon_js| {
                let fit_addon = fit_addon_js.clone().unchecked_into::<FitAddon>();
                f(&fit_addon)
            })
    })
}

fn setup_resize_listener() {
    USE_FIT.with(|use_fit| {
        if !*use_fit.get().unwrap() {
            return;
        }

        let window = web_sys::window().expect("no global window exists");

        let callback = Closure::wrap(
            Box::new(move || {
                if
                    let Some(_) = with_fit_addon(|addon| {
                        addon.fit();
                    })
                {
                    // FitAddon successfully applied
                }
            }) as Box<dyn FnMut()>
        );

        window
            .add_event_listener_with_callback("resize", callback.as_ref().unchecked_ref())
            .expect("failed to add resize listener");

        RESIZE_CALLBACK.with(|rc| {
            let _ = rc.set(callback);
        });
    });
}

/// Initialize the terminal with configuration options
pub fn init_terminal(
    options: &xterm_js_rs::TerminalOptions,
    parent: HtmlElement,
    config: TerminalConfig
) {
    TERMINAL.with(|t| {
        let (mut tx, rx) = mpsc::channel(32);
        let mut tx_ = tx.clone();
        let terminal = xterm_js_rs::Terminal::new(options);

        let callback = Closure::wrap(
            Box::new(move |e: xterm_js_rs::Event| {
                tx_.try_send(e.as_string().unwrap()).ok();
            }) as Box<dyn FnMut(_)>
        );
        terminal.on_data(callback.as_ref().unchecked_ref());
        callback.forget();

        let callback = Closure::wrap(
            Box::new(move |e: xterm_js_rs::Event| {
                tx.try_send(e.as_string().unwrap()).ok();
            }) as Box<dyn FnMut(_)>
        );
        terminal.on_binary(callback.as_ref().unchecked_ref());
        callback.forget();

        DATA_CHANNEL.set(Mutex::new(rx)).unwrap();

        // Set up FIT_ADDON based on configuration
        USE_FIT.with(|use_fit| {
            if use_fit.set(config.use_fit).is_err() {
                panic!("Failed to set USE_FIT");
            }
        });

        if config.use_fit {
            let fit_addon = FitAddon::new();

            FIT_ADDON.with(|fa| {
                if fa.set(Some(fit_addon.clone().into())).is_err() {
                    panic!("Failed to set FIT_ADDON");
                }
            });

            terminal.load_addon(fit_addon.into());
        } else {
            FIT_ADDON.with(|fa| {
                if fa.set(None).is_err() {
                    panic!("Failed to set FIT_ADDON to None");
                }
            });
        }

        // Load WebGL addon based on configuration
        if config.use_webgl {
            let webgl_addon = WebglAddon::new(Some(true));
            terminal.load_addon(webgl_addon.into());
        }

        terminal.open(parent);
        terminal.focus();

        if config.use_fit {
            with_fit_addon(|addon| {
                addon.fit();
            });
        }

        if t.set(terminal).is_err() {
            panic!("Failed to set TERMINAL");
        }

        if config.use_fit {
            setup_resize_listener();
        }
    });
}

pub(crate) fn poll_next_event(cx: &mut Context<'_>) -> Poll<Option<String>> {
    DATA_CHANNEL.get().unwrap().lock().unwrap().poll_next_unpin(cx)
}

pub fn window_size() -> io::Result<WindowSize> {
    Ok(
        with_terminal(|t| WindowSize {
            rows: t.get_rows() as u16,
            columns: t.get_cols() as u16,
            width: t.get_element().client_width() as u16,
            height: t.get_element().client_height() as u16,
        })
    )
}

pub(crate) fn size() -> io::Result<(u16, u16)> {
    window_size().map(|s| (s.columns, s.rows))
}

pub fn cursor_position() -> io::Result<(u16, u16)> {
    Ok(
        with_terminal(|t| {
            let active = t.get_buffer().get_active();
            (active.get_cursor_x() as u16, active.get_cursor_y() as u16)
        })
    )
}

pub fn perform_fit() -> io::Result<()> {
    match
        with_fit_addon(|addon| {
            addon.fit();
        })
    {
        Some(_) => Ok(()),
        None => Err(io::Error::new(io::ErrorKind::Unsupported, "FitAddon is not enabled")),
    }
}

#[derive(Default)]
pub struct TerminalHandle {
    buffer: RefCell<Vec<u8>>,
}

impl io::Write for TerminalHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buffer.borrow_mut().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let s = String::from_utf8(self.buffer.replace(Vec::new())).map_err(|e|
            io::Error::new(io::ErrorKind::InvalidData, e)
        )?;
        with_terminal(|t| t.write(&s));
        Ok(())
    }
}
