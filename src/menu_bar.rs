use std::sync::mpsc::{self, Receiver, Sender};

use eframe::egui;
use objc2::{
    ClassType, DeclaredClass, declare_class, msg_send, msg_send_id, mutability, rc::Retained, sel,
};
use objc2_app_kit::{
    NSApplication, NSImage, NSMenu, NSMenuItem, NSSquareStatusItemLength, NSStatusBar, NSStatusItem,
};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSString, ns_string};

use crate::config::AppConfig;

#[derive(Clone)]
pub enum MenuBarAction {
    Show,
    NewLocal,
    CheckForUpdates,
    ToggleWordWrap,
    PresentationSettings,
    OpenHost(String),
    Quit,
}

#[derive(Debug)]
struct MenuTargetIvars {
    sender: Sender<isize>,
    ctx: egui::Context,
}

declare_class!(
    struct MenuTarget;

    unsafe impl ClassType for MenuTarget {
        type Super = NSObject;
        type Mutability = mutability::MainThreadOnly;
        const NAME: &'static str = "ShellHopperMenuTarget";
    }

    impl DeclaredClass for MenuTarget {
        type Ivars = MenuTargetIvars;
    }

    unsafe impl NSObjectProtocol for MenuTarget {}

    unsafe impl MenuTarget {
        #[method(openSession:)]
        fn open_session(&self, item: &NSMenuItem) {
            MenuBar::activate_app();
            let _ = self.ivars().sender.send(unsafe { item.tag() });
            self.ivars().ctx.request_repaint();
        }
    }
);

impl MenuTarget {
    fn new(sender: Sender<isize>, ctx: egui::Context, mtm: MainThreadMarker) -> Retained<Self> {
        let this = mtm.alloc().set_ivars(MenuTargetIvars { sender, ctx });
        unsafe { msg_send_id![super(this), init] }
    }
}

pub struct MenuBar {
    _status_item: Retained<NSStatusItem>,
    menu: Retained<NSMenu>,
    target: Retained<MenuTarget>,
    receiver: Receiver<isize>,
    actions: Vec<MenuBarAction>,
}

impl MenuBar {
    pub fn new(config: &AppConfig, ctx: &egui::Context) -> Self {
        let mtm = MainThreadMarker::new().expect("menu bar must be created on the main thread");
        let (sender, receiver) = mpsc::channel();
        let target = MenuTarget::new(sender, ctx.clone(), mtm);
        let status_bar = unsafe { NSStatusBar::systemStatusBar() };
        let status_item = unsafe { status_bar.statusItemWithLength(NSSquareStatusItemLength) };
        let menu = unsafe { NSMenu::initWithTitle(mtm.alloc(), ns_string!("ShellHopper")) };

        if let Some(button) = unsafe { status_item.button(mtm) }
            && let Some(image) = unsafe {
                NSImage::imageWithSystemSymbolName_accessibilityDescription(
                    ns_string!("terminal"),
                    Some(ns_string!("ShellHopper sessions")),
                )
            }
        {
            unsafe {
                image.setTemplate(true);
                button.setImage(Some(&image));
            }
        }
        unsafe { status_item.setMenu(Some(&menu)) };

        let mut menu_bar = Self {
            _status_item: status_item,
            menu,
            target,
            receiver,
            actions: Vec::new(),
        };
        menu_bar.refresh(config);
        menu_bar
    }

    pub fn refresh(&mut self, config: &AppConfig) {
        unsafe { self.menu.removeAllItems() };
        self.actions.clear();

        self.add_action("Show ShellHopper", MenuBarAction::Show);
        self.add_action("New Local Terminal", MenuBarAction::NewLocal);
        self.add_action("Check for Updates…", MenuBarAction::CheckForUpdates);
        self.add_update_action_to_app_menu();
        self.add_settings_to_app_menu(config);
        self.menu
            .addItem(&NSMenuItem::separatorItem(MainThreadMarker::new().unwrap()));

        for group in &config.groups {
            let hosts: Vec<_> = config
                .hosts
                .iter()
                .filter(|host| host.group == *group)
                .collect();
            if hosts.is_empty() {
                continue;
            }

            let mtm = MainThreadMarker::new().unwrap();
            let group_item = unsafe {
                NSMenuItem::initWithTitle_action_keyEquivalent(
                    mtm.alloc(),
                    &NSString::from_str(group),
                    None,
                    ns_string!(""),
                )
            };
            let submenu = unsafe { NSMenu::initWithTitle(mtm.alloc(), &NSString::from_str(group)) };
            for host in hosts {
                Self::add_action_to(
                    &submenu,
                    &self.target,
                    &mut self.actions,
                    &host.name,
                    MenuBarAction::OpenHost(host.name.clone()),
                );
            }
            group_item.setSubmenu(Some(&submenu));
            self.menu.addItem(&group_item);
        }

        self.menu
            .addItem(&NSMenuItem::separatorItem(MainThreadMarker::new().unwrap()));
        self.add_action("Quit ShellHopper", MenuBarAction::Quit);
    }

    pub fn next_action(&self) -> Option<MenuBarAction> {
        let tag = self.receiver.try_recv().ok()?;
        self.actions.get(tag as usize).cloned()
    }

    pub fn activate_app() {
        let mtm = MainThreadMarker::new().expect("menu actions run on the main thread");
        let app = NSApplication::sharedApplication(mtm);
        unsafe { app.unhide(None) };
        for window in app.windows().iter() {
            if window.isMiniaturized() {
                unsafe { window.deminiaturize(None) };
            }
            window.makeKeyAndOrderFront(None);
        }
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }

    fn add_action(&mut self, title: &str, action: MenuBarAction) {
        Self::add_action_to(&self.menu, &self.target, &mut self.actions, title, action);
    }

    fn add_update_action_to_app_menu(&mut self) {
        let mtm = MainThreadMarker::new().unwrap();
        let Some(menu) = (unsafe {
            NSApplication::sharedApplication(mtm)
                .mainMenu()
                .and_then(|menu| menu.itemAtIndex(0))
                .and_then(|item| item.submenu())
        }) else {
            return;
        };
        if let Some(item) = unsafe { menu.itemWithTitle(ns_string!("Check for Updates…")) } {
            unsafe { menu.removeItem(&item) };
        }
        let item = Self::action_item(&self.target, self.actions.len(), "Check for Updates…");
        self.actions.push(MenuBarAction::CheckForUpdates);
        unsafe { menu.insertItem_atIndex(&item, 1.min(menu.numberOfItems())) };
    }

    fn add_settings_to_app_menu(&mut self, config: &AppConfig) {
        let mtm = MainThreadMarker::new().unwrap();
        let Some(menu) = (unsafe {
            NSApplication::sharedApplication(mtm)
                .mainMenu()
                .and_then(|menu| menu.itemAtIndex(0))
                .and_then(|item| item.submenu())
        }) else {
            return;
        };
        if let Some(item) = unsafe { menu.itemWithTitle(ns_string!("Settings")) } {
            unsafe { menu.removeItem(&item) };
        }
        let settings_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                ns_string!("Settings"),
                None,
                ns_string!(""),
            )
        };
        let submenu = unsafe { NSMenu::initWithTitle(mtm.alloc(), ns_string!("Settings")) };
        let word_wrap = Self::action_item(&self.target, self.actions.len(), "Word Wrap");
        self.actions.push(MenuBarAction::ToggleWordWrap);
        // NSControlStateValueOn (1) draws the checkmark; Off (0) draws nothing.
        let state: isize = if config.no_word_wrap { 0 } else { 1 };
        let _: () = unsafe { msg_send![&*word_wrap, setState: state] };
        submenu.addItem(&word_wrap);
        let presentation =
            Self::action_item(&self.target, self.actions.len(), "Presentation Mode…");
        self.actions.push(MenuBarAction::PresentationSettings);
        submenu.addItem(&presentation);
        settings_item.setSubmenu(Some(&submenu));
        unsafe { menu.insertItem_atIndex(&settings_item, 2.min(menu.numberOfItems())) };
    }

    fn add_action_to(
        menu: &NSMenu,
        target: &MenuTarget,
        actions: &mut Vec<MenuBarAction>,
        title: &str,
        action: MenuBarAction,
    ) {
        let item = Self::action_item(target, actions.len(), title);
        actions.push(action);
        menu.addItem(&item);
    }

    fn action_item(target: &MenuTarget, tag: usize, title: &str) -> Retained<NSMenuItem> {
        let mtm = MainThreadMarker::new().unwrap();
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(title),
                Some(sel!(openSession:)),
                ns_string!(""),
            )
        };
        unsafe {
            item.setTarget(Some(target));
            item.setTag(tag as isize);
        }
        item
    }
}
