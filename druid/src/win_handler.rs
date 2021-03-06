// Copyright 2019 The xi-editor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The implementation of the WinHandler trait (druid-shell integration).

use std::any::Any;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use log::{info, warn};

use crate::kurbo::{Size, Vec2};
use crate::piet::Piet;
use crate::shell::{
    Application, FileDialogOptions, IdleToken, MouseEvent, WinCtx, WinHandler, WindowHandle,
};

use crate::app_delegate::{AppDelegate, DelegateCtx};
use crate::core::CommandQueue;
use crate::ext_event::ExtEventHost;
use crate::menu::ContextMenu;
use crate::window::{PendingWindow, Window};
use crate::{
    Command, Data, Env, Event, KeyEvent, KeyModifiers, MenuDesc, Target, TimerToken, WheelEvent,
    WindowDesc, WindowId,
};

use crate::command::sys as sys_cmd;

pub(crate) const RUN_COMMANDS_TOKEN: IdleToken = IdleToken::new(1);

/// A token we are called back with if an external event was submitted.
pub(crate) const EXT_EVENT_IDLE_TOKEN: IdleToken = IdleToken::new(2);

/// The struct implements the druid-shell `WinHandler` trait.
///
/// One `DruidHandler` exists per window.
///
/// This is something of an internal detail and possibly we don't want to surface
/// it publicly.
pub struct DruidHandler<T: Data> {
    /// The shared app state.
    app_state: Rc<RefCell<AppState<T>>>,
    /// The id for the current window.
    window_id: WindowId,
}

/// State shared by all windows in the UI.
pub(crate) struct AppState<T: Data> {
    delegate: Option<Box<dyn AppDelegate<T>>>,
    command_queue: CommandQueue,
    ext_event_host: ExtEventHost,
    windows: Windows<T>,
    pub(crate) env: Env,
    pub(crate) data: T,
}

/// All active windows.
struct Windows<T: Data> {
    pending: HashMap<WindowId, PendingWindow<T>>,
    windows: HashMap<WindowId, Window<T>>,
}

impl<T: Data> Windows<T> {
    fn connect(&mut self, id: WindowId, handle: WindowHandle) {
        if let Some(pending) = self.pending.remove(&id) {
            let win = pending.into_window(id, handle);
            assert!(self.windows.insert(id, win).is_none(), "duplicate window");
        } else {
            log::error!("no window for connecting handle {:?}", id);
        }
    }

    fn add(&mut self, id: WindowId, win: PendingWindow<T>) {
        assert!(self.pending.insert(id, win).is_none(), "duplicate pending");
    }

    fn remove(&mut self, id: WindowId) -> Option<WindowHandle> {
        self.windows.remove(&id).map(|entry| entry.handle)
    }

    fn iter_mut(&mut self) -> impl Iterator<Item = &'_ mut Window<T>> {
        self.windows.values_mut()
    }

    fn get_mut(&mut self, id: WindowId) -> Option<&mut Window<T>> {
        self.windows.get_mut(&id)
    }
}

impl<T: Data> AppState<T> {
    pub(crate) fn new(
        data: T,
        env: Env,
        delegate: Option<Box<dyn AppDelegate<T>>>,
        ext_event_host: ExtEventHost,
    ) -> Rc<RefCell<Self>> {
        Rc::new(RefCell::new(AppState {
            delegate,
            command_queue: VecDeque::new(),
            ext_event_host,
            data,
            env,
            windows: Windows::default(),
        }))
    }

    fn get_menu_cmd(&self, window_id: WindowId, cmd_id: u32) -> Option<Command> {
        self.windows
            .windows
            .get(&window_id)
            .and_then(|w| w.get_menu_cmd(cmd_id))
    }

    /// A helper fn for setting up the `DelegateCtx`. Takes a closure with
    /// an arbitrary return type `R`, and returns `Some(R)` if an `AppDelegate`
    /// is configured.
    fn with_delegate<R, F>(&mut self, id: WindowId, f: F) -> Option<R>
    where
        F: FnOnce(&mut Box<dyn AppDelegate<T>>, &mut T, &Env, &mut DelegateCtx) -> R,
    {
        let AppState {
            ref mut delegate,
            ref mut command_queue,
            ref mut data,
            ref env,
            ..
        } = self;
        let mut ctx = DelegateCtx {
            source_id: id,
            command_queue,
        };
        if let Some(delegate) = delegate {
            Some(f(delegate, data, env, &mut ctx))
        } else {
            None
        }
    }

    fn delegate_event(&mut self, id: WindowId, event: Event) -> Option<Event> {
        if self.delegate.is_some() {
            self.with_delegate(id, |del, data, env, ctx| del.event(event, data, env, ctx))
                .unwrap()
        } else {
            Some(event)
        }
    }

    fn connect(&mut self, id: WindowId, handle: WindowHandle) {
        self.windows.connect(id, handle);

        // If the external event host has no handle, it cannot wake us
        // when an event arrives.
        if self.ext_event_host.handle_window_id.is_none() {
            self.set_ext_event_idle_handler(id);
        }

        self.with_delegate(id, |del, data, env, ctx| {
            del.window_added(id, data, env, ctx)
        });
    }

    pub(crate) fn add_window(&mut self, id: WindowId, window: PendingWindow<T>) {
        self.windows.add(id, window);
    }

    /// Called after this window has been closed by the platform.
    ///
    /// We clean up resources and notifiy the delegate, if necessary.
    fn remove_window(&mut self, window_id: WindowId, _ctx: &mut dyn WinCtx) {
        self.with_delegate(window_id, |del, data, env, ctx| {
            del.window_removed(window_id, data, env, ctx)
        });
        self.windows.remove(window_id);

        // if we are closing the window that is currently responsible for
        // waking us when external events arrive, we want to pass that responsibility
        // to another window.
        if self.ext_event_host.handle_window_id == Some(window_id) {
            self.ext_event_host.handle_window_id = None;
            // find any other live window
            let win_id = self.windows.windows.keys().find(|k| *k != &window_id);
            if let Some(any_other_window) = win_id.cloned() {
                self.set_ext_event_idle_handler(any_other_window);
            }
        }
    }

    /// Set the idle handle that will be used to wake us when external events arrive.
    fn set_ext_event_idle_handler(&mut self, id: WindowId) {
        if let Some(mut idle) = self
            .windows
            .get_mut(id)
            .and_then(|win| win.handle.get_idle_handle())
        {
            if self.ext_event_host.has_pending_items() {
                idle.schedule_idle(EXT_EVENT_IDLE_TOKEN);
            }
            self.ext_event_host.set_idle(idle, id);
        }
    }

    /// triggered by a menu item or other command.
    ///
    /// This doesn't close the window; it calls the close method on the platform
    /// window handle; the platform should close the window, and then call
    /// our handlers `destroy()` method, at which point we can do our cleanup.
    fn request_close_window(&mut self, window_id: WindowId) {
        if let Some(win) = self.windows.get_mut(window_id) {
            win.handle.close();
        }
    }

    fn show_window(&mut self, id: WindowId) {
        if let Some(win) = self.windows.get_mut(id) {
            win.handle.bring_to_front_and_focus();
        }
    }

    /// Returns `true` if an animation frame was requested.
    fn paint(&mut self, window_id: WindowId, piet: &mut Piet, _ctx: &mut dyn WinCtx) -> bool {
        if let Some(win) = self.windows.get_mut(window_id) {
            win.do_paint(piet, &mut self.command_queue, &self.data, &self.env);
            win.wants_animation_frame()
        } else {
            false
        }
    }

    fn do_event(&mut self, source_id: WindowId, event: Event, win_ctx: &mut dyn WinCtx) -> bool {
        // if the event was swallowed by the delegate we consider it handled?
        let event = match self.delegate_event(source_id, event) {
            Some(event) => event,
            None => return true,
        };

        if let Event::TargetedCommand(_target, ref cmd) = event {
            match cmd.selector {
                sys_cmd::SET_MENU => {
                    self.set_menu(source_id, cmd);
                    return true;
                }
                sys_cmd::SHOW_CONTEXT_MENU => {
                    self.show_context_menu(source_id, cmd);
                    return true;
                }
                _ => (),
            }
        }

        let AppState {
            ref mut command_queue,
            ref mut windows,
            ref mut data,
            ref env,
            ..
        } = self;

        match event {
            Event::TargetedCommand(Target::Widget(_), _) => {
                let mut any_handled = false;

                // TODO: this is using the WinCtx of the window originating the event,
                // rather than a WinCtx appropriate to the target window. This probably
                // needs to get rethought.
                for window in windows.iter_mut() {
                    let handled = window.event(win_ctx, command_queue, event.clone(), data, env);
                    any_handled |= handled;
                    if handled {
                        break;
                    }
                }
                any_handled
            }
            _ => match windows.get_mut(source_id) {
                Some(win) => win.event(win_ctx, command_queue, event, data, env),
                None => false,
            },
        }
    }

    fn set_menu(&mut self, window_id: WindowId, cmd: &Command) {
        if let Some(win) = self.windows.get_mut(window_id) {
            match cmd.get_object::<MenuDesc<T>>() {
                Ok(menu) => win.set_menu(menu.to_owned(), &self.data, &self.env),
                Err(e) => log::warn!("set-menu object error: '{}'", e),
            }
        }
    }

    fn show_context_menu(&mut self, window_id: WindowId, cmd: &Command) {
        if let Some(win) = self.windows.get_mut(window_id) {
            match cmd.get_object::<ContextMenu<T>>() {
                Ok(ContextMenu { menu, location }) => {
                    win.show_context_menu(menu.to_owned(), *location, &self.data, &self.env)
                }
                Err(e) => log::warn!("show-context-menu object error: '{}'", e),
            }
        }
    }

    fn do_update(&mut self, win_ctx: &mut dyn WinCtx) {
        // we send `update` to all windows, not just the active one:
        for window in self.windows.iter_mut() {
            window.update(win_ctx, &self.data, &self.env);
        }
        self.invalidate_and_finalize();
    }

    /// invalidate any window handles that need it.
    ///
    /// This should always be called at the end of an event update cycle,
    /// including for lifecycle events.
    fn invalidate_and_finalize(&mut self) {
        for win in self.windows.iter_mut() {
            win.invalidate_and_finalize(&mut self.command_queue, &self.data, &self.env);
        }
    }

    #[cfg(target_os = "macos")]
    fn window_got_focus(&mut self, window_id: WindowId) {
        if let Some(win) = self.windows.get_mut(window_id) {
            win.macos_update_app_menu(&self.data, &self.env)
        }
    }
    #[cfg(not(target_os = "macos"))]
    fn window_got_focus(&mut self, _: WindowId) {}
}

impl<T: Data> DruidHandler<T> {
    /// Note: the root widget doesn't go in here, because it gets added to the
    /// app state.
    pub(crate) fn new_shared(
        app_state: Rc<RefCell<AppState<T>>>,
        window_id: WindowId,
    ) -> DruidHandler<T> {
        DruidHandler {
            app_state,
            window_id,
        }
    }

    /// Send an event to the widget hierarchy.
    ///
    /// Returns `true` if the event produced an action.
    ///
    /// This is principally because in certain cases (such as keydown on Windows)
    /// the OS needs to know if an event was handled.
    fn do_event(&mut self, event: Event, win_ctx: &mut dyn WinCtx) -> bool {
        let result = self
            .app_state
            .borrow_mut()
            .do_event(self.window_id, event, win_ctx);
        self.process_commands(win_ctx);
        self.app_state.borrow_mut().do_update(win_ctx);
        result
    }

    fn process_commands(&mut self, win_ctx: &mut dyn WinCtx) {
        loop {
            let next_cmd = self.app_state.borrow_mut().command_queue.pop_front();
            match next_cmd {
                Some((target, cmd)) => self.handle_cmd(target, cmd, win_ctx),
                None => break,
            }
        }
    }

    fn process_ext_events(&mut self, win_ctx: &mut dyn WinCtx) {
        loop {
            let ext_cmd = self.app_state.borrow_mut().ext_event_host.recv();
            match ext_cmd {
                Some((targ, cmd)) => {
                    let targ = targ.unwrap_or_else(|| self.window_id.into());
                    self.handle_cmd(targ, cmd, win_ctx);
                }
                None => break,
            }
        }
        self.app_state.borrow_mut().invalidate_and_finalize();
    }

    fn handle_system_cmd(&mut self, cmd_id: u32, win_ctx: &mut dyn WinCtx) {
        let cmd = self.app_state.borrow().get_menu_cmd(self.window_id, cmd_id);
        match cmd {
            Some(cmd) => self
                .app_state
                .borrow_mut()
                .command_queue
                .push_back((self.window_id.into(), cmd)),
            None => warn!("No command for menu id {}", cmd_id),
        }
        self.process_commands(win_ctx)
    }

    /// Handle a command. Top level commands (e.g. for creating and destroying windows)
    /// have their logic here; other commands are passed to the window.
    fn handle_cmd(&mut self, target: Target, cmd: Command, win_ctx: &mut dyn WinCtx) {
        //FIXME: we need some way of getting the correct `WinCtx` for this window.
        if let Target::Window(window_id) = target {
            match &cmd.selector {
                &sys_cmd::SHOW_OPEN_PANEL => self.show_open_panel(cmd, window_id, win_ctx),
                &sys_cmd::SHOW_SAVE_PANEL => self.show_save_panel(cmd, window_id, win_ctx),
                &sys_cmd::NEW_WINDOW => {
                    if let Err(e) = self.new_window(cmd) {
                        log::error!("failed to create window: '{}'", e);
                    }
                }
                &sys_cmd::CLOSE_WINDOW => self.request_close_window(cmd, window_id),
                &sys_cmd::SHOW_WINDOW => self.show_window(cmd),
                &sys_cmd::QUIT_APP => self.quit(),
                &sys_cmd::HIDE_APPLICATION => self.hide_app(),
                &sys_cmd::HIDE_OTHERS => self.hide_others(),
                &sys_cmd::PASTE => self.do_paste(window_id, win_ctx),
                sel => {
                    info!("handle_cmd {}", sel);
                    let event = Event::TargetedCommand(target, cmd);
                    self.app_state
                        .borrow_mut()
                        .do_event(window_id, event, win_ctx);
                }
            }
        } else {
            info!("handle_cmd {} -> widget", cmd.selector);
            let event = Event::TargetedCommand(target, cmd);
            // TODO: self.window_id the correct source identifier here?
            self.app_state
                .borrow_mut()
                .do_event(self.window_id, event, win_ctx);
        }
    }

    fn show_open_panel(&mut self, cmd: Command, window_id: WindowId, win_ctx: &mut dyn WinCtx) {
        let options = cmd
            .get_object::<FileDialogOptions>()
            .map(|opts| opts.to_owned())
            .unwrap_or_default();
        let result = win_ctx.open_file_sync(options);
        if let Some(info) = result {
            let cmd = Command::new(sys_cmd::OPEN_FILE, info);
            let event = Event::TargetedCommand(window_id.into(), cmd);
            self.app_state
                .borrow_mut()
                .do_event(window_id, event, win_ctx);
        }
    }

    fn show_save_panel(&mut self, cmd: Command, window_id: WindowId, win_ctx: &mut dyn WinCtx) {
        let options = cmd
            .get_object::<FileDialogOptions>()
            .map(|opts| opts.to_owned())
            .unwrap_or_default();
        let result = win_ctx.save_as_sync(options);
        if let Some(info) = result {
            let cmd = Command::new(sys_cmd::SAVE_FILE, info);
            let event = Event::TargetedCommand(window_id.into(), cmd);
            self.app_state
                .borrow_mut()
                .do_event(window_id, event, win_ctx);
        }
    }

    fn new_window(&mut self, cmd: Command) -> Result<(), Box<dyn std::error::Error>> {
        let desc = cmd.take_object::<WindowDesc<T>>()?;
        let window = desc.build_native(&self.app_state)?;
        window.show();
        Ok(())
    }

    fn request_close_window(&mut self, cmd: Command, window_id: WindowId) {
        let id = cmd.get_object().unwrap_or(&window_id);
        self.app_state.borrow_mut().request_close_window(*id);
    }

    fn show_window(&mut self, cmd: Command) {
        let id: WindowId = *cmd
            .get_object()
            .expect("show window selector missing window id");
        self.app_state.borrow_mut().show_window(id);
    }

    fn do_paste(&mut self, window_id: WindowId, ctx: &mut dyn WinCtx) {
        let event = Event::Paste(Application::clipboard());
        self.app_state.borrow_mut().do_event(window_id, event, ctx);
    }

    fn quit(&self) {
        Application::quit()
    }

    fn hide_app(&self) {
        #[cfg(all(target_os = "macos", not(feature = "use_gtk")))]
        Application::hide()
    }

    fn hide_others(&mut self) {
        #[cfg(all(target_os = "macos", not(feature = "use_gtk")))]
        Application::hide_others()
    }
}

impl<T: Data> WinHandler for DruidHandler<T> {
    fn connect(&mut self, handle: &WindowHandle) {
        //NOTE: this method predates `connected`, and we call delegate methods here.
        //it's possible that we should move those calls to occur in connected?
        self.app_state
            .borrow_mut()
            .connect(self.window_id, handle.clone());
    }

    fn connected(&mut self, ctx: &mut dyn WinCtx) {
        let event = Event::WindowConnected;
        self.do_event(event, ctx);
    }

    fn paint(&mut self, piet: &mut Piet, ctx: &mut dyn WinCtx) -> bool {
        self.app_state.borrow_mut().paint(self.window_id, piet, ctx)
    }

    fn size(&mut self, width: u32, height: u32, ctx: &mut dyn WinCtx) {
        let event = Event::Size(Size::new(f64::from(width), f64::from(height)));
        self.do_event(event, ctx);
    }

    fn command(&mut self, id: u32, ctx: &mut dyn WinCtx) {
        self.handle_system_cmd(id, ctx);
    }

    fn mouse_down(&mut self, event: &MouseEvent, ctx: &mut dyn WinCtx) {
        // TODO: double-click detection (or is this done in druid-shell?)
        let event = Event::MouseDown(event.clone().into());
        self.do_event(event, ctx);
    }

    fn mouse_up(&mut self, event: &MouseEvent, ctx: &mut dyn WinCtx) {
        let event = Event::MouseUp(event.clone().into());
        self.do_event(event, ctx);
    }

    fn mouse_move(&mut self, event: &MouseEvent, ctx: &mut dyn WinCtx) {
        let event = Event::MouseMoved(event.clone().into());
        self.do_event(event, ctx);
    }

    fn key_down(&mut self, event: KeyEvent, ctx: &mut dyn WinCtx) -> bool {
        self.do_event(Event::KeyDown(event), ctx)
    }

    fn key_up(&mut self, event: KeyEvent, ctx: &mut dyn WinCtx) {
        self.do_event(Event::KeyUp(event), ctx);
    }

    fn wheel(&mut self, delta: Vec2, mods: KeyModifiers, ctx: &mut dyn WinCtx) {
        let event = Event::Wheel(WheelEvent { delta, mods });
        self.do_event(event, ctx);
    }

    fn zoom(&mut self, delta: f64, ctx: &mut dyn WinCtx) {
        let event = Event::Zoom(delta);
        self.do_event(event, ctx);
    }

    fn got_focus(&mut self, _ctx: &mut dyn WinCtx) {
        self.app_state.borrow_mut().window_got_focus(self.window_id);
    }

    fn timer(&mut self, token: TimerToken, ctx: &mut dyn WinCtx) {
        self.do_event(Event::Timer(token), ctx);
    }

    fn idle(&mut self, token: IdleToken, ctx: &mut dyn WinCtx) {
        match token {
            RUN_COMMANDS_TOKEN => {
                self.process_commands(ctx);
                self.app_state.borrow_mut().invalidate_and_finalize();
            }
            EXT_EVENT_IDLE_TOKEN => self.process_ext_events(ctx),
            other => log::warn!("unexpected idle token {:?}", other),
        }
    }

    fn as_any(&mut self) -> &mut dyn Any {
        self
    }

    fn destroy(&mut self, ctx: &mut dyn WinCtx) {
        self.app_state
            .borrow_mut()
            .remove_window(self.window_id, ctx);
    }
}

impl<T: Data> Default for Windows<T> {
    fn default() -> Self {
        Windows {
            windows: HashMap::new(),
            pending: HashMap::new(),
        }
    }
}
