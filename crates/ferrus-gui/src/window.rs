//! Ferrus main window: Rufus's control set, Adwaita presentation.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use adw::{
    ActionRow, ApplicationWindow, ComboRow, EntryRow, HeaderBar, PreferencesGroup, SwitchRow,
    ToolbarView, WindowTitle,
};
use gtk::glib;
use gtk::glib::ControlFlow;
use gtk::{gio, Button, Label, Orientation, PolicyType, ProgressBar, ScrolledWindow, StringList};

use ferrus_core::client::{self, ClientEvent, FlashHandle};
use ferrus_core::device::{self, BlockDevice};
use ferrus_core::protocol::{
    BadBlocks, FlashPlan, PartitionScheme as FlashPlanScheme, WinOptions,
};
use std::sync::mpsc::TryRecvError;

const BOOT_IMAGE: u32 = 1;

#[derive(Clone)]
pub struct Window {
    inner: Rc<Inner>,
}

struct Inner {
    window: ApplicationWindow,
    device_combo: ComboRow,
    devices: RefCell<Vec<BlockDevice>>,
    refresh_btn: Button,
    hd_switch: SwitchRow,
    boot_combo: ComboRow,
    image_row: ActionRow,
    select_btn: Button,
    scheme_combo: ComboRow,
    target_combo: ComboRow,
    label_entry: EntryRow,
    fs_combo: ComboRow,
    cluster_combo: ComboRow,
    bios_fixes: SwitchRow,
    quick_format: SwitchRow,
    bad_blocks: ComboRow,
    verify_switch: SwitchRow,
    /// Live-USB persistence slider (raw-DD plans only).
    persist_row: adw::SpinRow,
    /// Rufus's Windows user-experience switches (Win plans only).
    win_group: PreferencesGroup,
    opt_hw: SwitchRow,
    opt_account: SwitchRow,
    opt_bitlocker: SwitchRow,
    /// Windows To Go (Windows plans only).
    wtg_switch: SwitchRow,
    wtg_index: adw::SpinRow,
    status_label: Label,
    progress: ProgressBar,
    start_btn: Button,
    cancel_btn: Button,
    flash: RefCell<Option<FlashHandle>>,
    busy: Cell<bool>,
    iso_path: RefCell<Option<String>>,
    /// Flashing plan chosen by the background image probe.
    plan: RefCell<Option<FlashPlan>>,
    /// Persistence label the probed image expects (`casper-rw`/`persistence`).
    flavor: RefCell<Option<String>>,
    probe: Cell<ProbeState>,
}

#[derive(Clone, Copy, PartialEq)]
enum ProbeState {
    None,
    Running,
    Done,
    Failed,
}

impl Window {
    pub fn new(app: &adw::Application) -> Self {
        let window = ApplicationWindow::builder()
            .application(app)
            .title("Ferrus")
            .default_width(540)
            .default_height(720)
            .icon_name("media-removable-symbolic")
            .build();

        // ---------- header ----------
        let header = HeaderBar::new();
        header.set_title_widget(Some(&WindowTitle::new(
            "Ferrus",
            "Bootable USB creator for Linux",
        )));
        let about_btn = Button::from_icon_name("help-about-symbolic");
        about_btn.set_tooltip_text(Some("About Ferrus"));
        {
            let win = window.clone();
            about_btn.connect_clicked(move |_| show_about(&win));
        }
        header.pack_end(&about_btn);

        // ---------- device group ----------
        let device_combo = ComboRow::new();
        device_combo.set_title("Device");
        device_combo.set_subtitle("Select the drive to erase and flash");
        let refresh_btn = Button::from_icon_name("view-refresh-symbolic");
        refresh_btn.set_tooltip_text(Some("Rescan devices"));
        refresh_btn.add_css_class("flat");
        device_combo.add_suffix(&refresh_btn);

        let hd_switch = SwitchRow::new();
        hd_switch.set_title("List USB Hard Drives");
        hd_switch.set_subtitle("Include non-removable USB disks in the list");

        let device_group = PreferencesGroup::new();
        device_group.set_title("Device");
        device_group.add(&device_combo);
        device_group.add(&hd_switch);

        // ---------- boot selection group ----------
        let boot_combo = ComboRow::new();
        boot_combo.set_title("Boot selection");
        boot_combo.set_model(Some(&StringList::new(&[
            "Non bootable",
            "Disk or ISO image (Please select)",
        ])));
        boot_combo.set_selected(0);

        let select_btn = Button::with_label("SELECT…");
        select_btn.add_css_class("flat");
        boot_combo.add_suffix(&select_btn);

        let image_row = ActionRow::new();
        image_row.set_visible(false);

        let boot_group = PreferencesGroup::new();
        boot_group.set_title("Boot selection");
        boot_group.add(&boot_combo);
        boot_group.add(&image_row);

        // ---------- configuration group ----------
        let scheme_combo = ComboRow::new();
        scheme_combo.set_title("Partition scheme");
        scheme_combo.set_model(Some(&StringList::new(&[
            "GPT · for UEFI (non CSM)",
            "MBR · for BIOS or UEFI-CSM",
        ])));

        let target_combo = ComboRow::new();
        target_combo.set_title("Target system");
        set_combo_strings(
            &target_combo,
            &["UEFI (non CSM)", "BIOS (or UEFI-CSM)", "BIOS or UEFI"],
        );

        let label_entry = EntryRow::new();
        label_entry.set_title("Volume label");

        let fs_combo = ComboRow::new();
        fs_combo.set_title("File system");
        set_combo_strings(
            &fs_combo,
            &["FAT32", "NTFS", "exFAT", "UDF", "ext2", "ext3", "ext4"],
        );
        fs_combo.set_subtitle("Default is chosen automatically per image type");

        let cluster_combo = ComboRow::new();
        cluster_combo.set_title("Cluster size");

        let bios_fixes = SwitchRow::new();
        bios_fixes.set_title("Add fixes for old BIOSes");
        bios_fixes.set_subtitle("Extra partition alignment for very old systems");

        let config_group = PreferencesGroup::new();
        config_group.set_title("Configuration");
        config_group.add(&scheme_combo);
        config_group.add(&target_combo);
        config_group.add(&label_entry);
        config_group.add(&fs_combo);
        config_group.add(&cluster_combo);
        config_group.add(&bios_fixes);

        let persist_row = adw::SpinRow::with_range(0.0, 131_072.0, 512.0);
        persist_row.set_title("Persistent partition size (MiB)");
        persist_row.set_subtitle("Extra storage that survives reboots on live USBs");
        persist_row.set_visible(false);
        config_group.add(&persist_row);

        // ---------- Windows user experience group ----------
        let wtg_switch = SwitchRow::new();
        wtg_switch.set_title("Windows To Go");
        wtg_switch.set_subtitle("Boot the full Windows desktop from the stick itself");

        let wtg_index = adw::SpinRow::with_range(0.0, 99.0, 1.0);
        wtg_index.set_title("WIM edition index");
        wtg_index.set_subtitle("0 = first edition in install.wim/esd");
        wtg_index.set_visible(false);

        let opt_hw = SwitchRow::new();
        opt_hw.set_title("Remove hardware requirement checks");
        opt_hw.set_subtitle("TPM 2.0, Secure Boot and RAM/CPU/storage minimums");

        let opt_account = SwitchRow::new();
        opt_account.set_title("Skip the forced online account");
        opt_account.set_subtitle("Allow a local account during OOBE");

        let opt_bitlocker = SwitchRow::new();
        opt_bitlocker.set_title("Disable BitLocker auto-encryption");
        opt_bitlocker.set_subtitle("Keep new drives unencrypted at first logon");

        let win_group = PreferencesGroup::new();
        win_group.set_title("Windows user experience");
        win_group.add(&wtg_switch);
        win_group.add(&wtg_index);
        win_group.add(&opt_hw);
        win_group.add(&opt_account);
        win_group.add(&opt_bitlocker);
        win_group.set_visible(false);

        // ---------- format options group ----------
        let quick_format = SwitchRow::new();
        quick_format.set_title("Quick format");
        quick_format.set_active(true);

        let bad_blocks = ComboRow::new();
        bad_blocks.set_title("Bad blocks check");
        set_combo_strings(
            &bad_blocks,
            &["Disabled", "Fast (1 pass)", "Thorough (4 passes)"],
        );

        let verify_switch = SwitchRow::new();
        verify_switch.set_title("Verify written data");
        verify_switch.set_subtitle("Read back after writing and compare byte-for-byte");
        verify_switch.set_active(true);

        let format_group = PreferencesGroup::new();
        format_group.set_title("Format options");
        format_group.add(&quick_format);
        format_group.add(&bad_blocks);
        format_group.add(&verify_switch);

        // ---------- footer ----------
        let progress = ProgressBar::new();
        progress.set_visible(false);
        progress.set_show_text(true);
        let status_label = Label::new(Some("Ready."));
        status_label.set_halign(gtk::Align::Start);
        status_label.add_css_class("dimmed");

        let close_btn = Button::with_label("CLOSE");
        close_btn.set_action_name(Some("window.close"));

        let start_btn = Button::with_label("START");
        start_btn.add_css_class("pill");
        start_btn.add_css_class("suggested-action");

        let cancel_btn = Button::with_label("CANCEL");
        cancel_btn.add_css_class("pill");
        cancel_btn.add_css_class("destructive-action");
        cancel_btn.set_visible(false);

        let btn_row = gtk::Box::new(Orientation::Horizontal, 12);
        btn_row.set_halign(gtk::Align::End);
        btn_row.append(&close_btn);
        btn_row.append(&cancel_btn);
        btn_row.append(&start_btn);

        // ---------- assemble ----------
        let content = gtk::Box::new(Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(18);
        content.set_margin_start(12);
        content.set_margin_end(12);
        content.set_valign(gtk::Align::Start);
        content.append(&device_group);
        content.append(&boot_group);
        content.append(&config_group);
        content.append(&win_group);
        content.append(&format_group);
        content.append(&progress);
        content.append(&status_label);
        content.append(&btn_row);

        let scroll = ScrolledWindow::new();
        scroll.set_child(Some(&content));
        scroll.set_policy(PolicyType::Never, PolicyType::Automatic);
        scroll.set_vexpand(true);

        let toolbar = ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&scroll));
        window.set_content(Some(&toolbar));

        let inner = Rc::new(Inner {
            window: window.clone(),
            device_combo,
            devices: RefCell::new(Vec::new()),
            refresh_btn,
            hd_switch,
            boot_combo,
            image_row,
            select_btn,
            scheme_combo,
            target_combo,
            label_entry,
            fs_combo,
            cluster_combo,
            bios_fixes,
            quick_format,
            bad_blocks,
            verify_switch,
            persist_row,
            win_group,
            opt_hw,
            opt_account,
            opt_bitlocker,
            wtg_switch,
            wtg_index,
            status_label,
            progress,
            start_btn,
            cancel_btn,
            flash: RefCell::new(None),
            busy: Cell::new(false),
            iso_path: RefCell::new(None),
            plan: RefCell::new(None),
            flavor: RefCell::new(None),
            probe: Cell::new(ProbeState::None),
        });

        // ---------- signals ----------
        let me = inner.clone();
        inner.refresh_btn.connect_clicked(move |_| {
            Self::static_refresh_devices(&me);
        });

        let me = inner.clone();
        inner.hd_switch.connect_active_notify(move |_| {
            Self::static_refresh_devices(&me);
        });

        let me = inner.clone();
        inner.wtg_switch.connect_active_notify(move |sw| {
            me.wtg_index.set_visible(sw.is_active());
            Self::static_update_sensitivity(&me);
        });

        let me = inner.clone();
        inner.device_combo.connect_selected_notify(move |_| {
            Self::static_update_sensitivity(&me);
        });

        let me = inner.clone();
        inner.boot_combo.connect_selected_notify(move |combo| {
            if combo.selected() == BOOT_IMAGE && me.iso_path.borrow().is_none() {
                Self::static_open_image_dialog(&me);
            }
            Self::static_update_sensitivity(&me);
        });

        let me = inner.clone();
        inner.select_btn.connect_clicked(move |_| {
            Self::static_open_image_dialog(&me);
        });

        let me = inner.clone();
        inner.scheme_combo.connect_selected_notify(move |_| {
            Self::static_update_sensitivity(&me);
        });

        let me = inner.clone();
        inner.fs_combo.connect_selected_notify(move |_| {
            Self::static_populate_clusters(&me);
        });
        Self::static_populate_clusters(&inner);

        let me = inner.clone();
        inner.start_btn.connect_clicked(move |_| Self::static_on_start(&me));

        let me = inner.clone();
        inner.cancel_btn.connect_clicked(move |_| {
            if let Some(h) = me.flash.borrow().as_ref() {
                h.cancel();
            }
            me.status_label.set_text("Cancelling…");
        });

        Self::static_refresh_devices(&inner);

        Self { inner }
    }

    pub fn present_root(&self) {
        self.inner.window.present();
    }

    fn static_refresh_devices(me: &Rc<Inner>) {
        if me.busy.get() {
            return;
        }
        let all = match device::list_block_devices() {
            Ok(list) => list,
            Err(e) => {
                me.status_label
                    .set_text(&format!("Device scan failed: {e:#}"));
                return;
            }
        };
        let include_hd = me.hd_switch.is_active();
        let shown: Vec<BlockDevice> = all
            .into_iter()
            .filter(|d| d.removable || d.is_usb() || include_hd)
            .collect();

        let empty = shown.is_empty();
        let strings: Vec<String> = if empty {
            vec!["No removable devices found".to_string()]
        } else {
            shown.iter().map(BlockDevice::display_name).collect()
        };

        me.device_combo.set_model(Some(&StringList::new(
            &strings.iter().map(String::as_str).collect::<Vec<_>>(),
        )));
        *me.devices.borrow_mut() = shown;

        Self::static_update_sensitivity(me);
        me.status_label.set_text("Ready.");
    }

    fn selected_device(me: &Rc<Inner>) -> Option<BlockDevice> {
        let idx = me.device_combo.selected() as usize;
        me.devices.borrow().get(idx).cloned()
    }

    fn static_update_sensitivity(me: &Rc<Inner>) {
        let busy = me.busy.get();
        let has_entries = !me.devices.borrow().is_empty();
        let non_boot = me.boot_combo.selected() == 0;
        let img_mode = me.boot_combo.selected() == BOOT_IMAGE;
        let gpt = me.scheme_combo.selected() == 0;

        me.refresh_btn.set_sensitive(!busy);
        me.hd_switch.set_sensitive(!busy);
        me.device_combo.set_sensitive(!busy && has_entries);
        me.boot_combo.set_sensitive(!busy);
        me.select_btn.set_sensitive(!busy && img_mode);
        me.scheme_combo.set_sensitive(!busy);
        me.target_combo.set_sensitive(!busy && !gpt);
        me.label_entry.set_sensitive(!busy);
        me.fs_combo.set_sensitive(!busy);
        me.cluster_combo.set_sensitive(!busy);
        me.bios_fixes.set_sensitive(!busy);
        me.quick_format.set_sensitive(!busy);
        me.bad_blocks.set_sensitive(!busy);
        me.verify_switch.set_sensitive(!busy);
        me.persist_row.set_sensitive(!busy);
        me.opt_hw.set_sensitive(!busy);
        me.opt_account.set_sensitive(!busy);
        me.opt_bitlocker.set_sensitive(!busy);
        me.wtg_switch.set_sensitive(!busy);
        me.wtg_index.set_sensitive(!busy);

        let probe_ok = matches!(
            me.probe.get(),
            ProbeState::Done | ProbeState::Failed | ProbeState::None
        );
        let ready = !busy
            && has_entries
            && (non_boot
                || (img_mode
                    && me.iso_path.borrow().is_some()
                    && probe_ok
                    && me.plan.borrow().is_some()));
        me.start_btn.set_sensitive(ready);
        me.cancel_btn.set_visible(busy);
    }

    fn static_open_image_dialog(me: &Rc<Inner>) {
        let filter = gtk::FileFilter::new();
        filter.set_name(Some("Disk images (ISO, IMG)"));
        filter.add_mime_type("application/x-iso9660-image");
        for pattern in ["*.iso", "*.img", "*.raw"] {
            filter.add_pattern(pattern);
        }
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);

        let dialog = gtk::FileDialog::new();
        dialog.set_title("Select a disk or ISO image");
        dialog.set_filters(Some(&filters));

        let me2 = me.clone();
        glib::spawn_future_local(async move {
            if let Ok(file) = dialog.open_future(Some(&me2.window)).await {
                if let Some(path) = file.path() {
                    Self::static_set_image(&me2, &path.display().to_string());
                }
            }
        });
    }

    fn static_set_image(me: &Rc<Inner>, path: &str) {
        *me.iso_path.borrow_mut() = Some(path.to_string());
        *me.plan.borrow_mut() = None;
        *me.flavor.borrow_mut() = None;
        me.persist_row.set_value(0.0);
        me.probe.set(ProbeState::Running);

        let name = std::path::Path::new(path)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("IMAGE");
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);

        me.image_row.set_title(name);
        me.image_row
            .set_subtitle(&format!("{path} · {}", fmt_size(size)));
        me.image_row.set_visible(true);

        let sanitized: String = name
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(16)
            .collect::<String>()
            .to_uppercase();
        if !sanitized.is_empty() {
            me.label_entry.set_text(&sanitized);
        }

        me.status_label.set_text("Analyzing image…");
        Self::static_update_sensitivity(me);
        Self::static_start_probe(me, path);
    }

    /// Probe the selected image in a background thread; results land on the
    /// main loop through a channel + timeout poller (GTK is not thread-safe).
    fn static_start_probe(me: &Rc<Inner>, path: &str) {
        let (tx, rx) = std::sync::mpsc::channel::<Result<(FlashPlan, Option<String>), String>>();
        let path = path.to_string();
        std::thread::spawn(move || {
            let result = client::resolve_helper_path()
                .ok_or_else(|| "ferrus-helper not found".to_string())
                .and_then(|helper| {
                    client::decide_plan(&helper, std::path::Path::new(&path))
                        .map(|(plan, manifest)| (plan, manifest.linux_flavor.clone()))
                        .map_err(|e| format!("{e:#}"))
                });
            let _ = tx.send(result);
        });

        let me2 = me.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(80), move || {
            match rx.try_recv() {
                Ok(Ok((plan, flavor))) => {
                    me2.status_label.set_text(&format!(
                        "Detected: {} — ready to flash.",
                        plan.describe()
                    ));
                    *me2.flavor.borrow_mut() = flavor;
                    *me2.plan.borrow_mut() = Some(plan);
                    me2.probe.set(ProbeState::Done);
                    Self::static_update_dynamic_rows(&me2);
                    Self::static_update_sensitivity(&me2);
                    glib::ControlFlow::Break
                }
                Ok(Err(e)) => {
                    me2.status_label
                        .set_text(&format!("Image analysis failed: {e}"));
                    me2.probe.set(ProbeState::Failed);
                    Self::static_update_sensitivity(&me2);
                    glib::ControlFlow::Break
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
            }
        });
    }

    /// Show the persistence slider for raw-DD plans with a detected live
    /// flavour, and the Windows switches for Windows plans.
    fn static_update_dynamic_rows(me: &Rc<Inner>) {
        match me.plan.borrow().as_ref() {
            Some(FlashPlan::RawDd { .. }) => {
                me.persist_row
                    .set_visible(me.flavor.borrow().is_some());
                me.win_group.set_visible(false);
            }
            Some(FlashPlan::WinFat32 { .. } | FlashPlan::WinUefiNtfs { .. }) => {
                me.persist_row.set_visible(false);
                me.win_group.set_visible(true);
            }
            _ => {
                me.persist_row.set_visible(false);
                me.win_group.set_visible(false);
            }
        }
    }

    fn static_populate_clusters(me: &Rc<Inner>) {
        let fs = combo_text(&me.fs_combo).unwrap_or_default();
        let options: &[&str] = match fs.as_str() {
            "FAT32" => &[
                "Default (4096)",
                "512",
                "1024",
                "2048",
                "8192",
                "16384",
                "32768",
                "65536",
            ],
            "NTFS" => &[
                "Default (4096)",
                "512",
                "1024",
                "2048",
                "8192",
                "16384",
                "32768",
                "65536",
            ],
            "exFAT" => &[
                "Default (128 KB)",
                "512",
                "1024",
                "2048",
                "4096",
                "32768",
                "65536",
                "131072",
            ],
            "UDF" => &["Default", "512", "1024", "2048", "4096"],
            _ => &["Default (4096)", "1024", "2048", "4096"],
        };
        me.cluster_combo.set_model(Some(&StringList::new(options)));
        me.cluster_combo.set_selected(0);
    }

    fn static_chosen_scheme(me: &Rc<Inner>) -> FlashPlanScheme {
        if me.scheme_combo.selected() == 0 {
            FlashPlanScheme::Gpt
        } else {
            FlashPlanScheme::Mbr
        }
    }

    fn static_bad_blocks_choice(me: &Rc<Inner>) -> Option<BadBlocks> {
        match me.bad_blocks.selected() {
            1 => Some(BadBlocks::Fast),
            2 => Some(BadBlocks::Thorough),
            _ => None,
        }
    }

    fn static_cluster_choice(me: &Rc<Inner>) -> Option<u64> {
        combo_text(&me.cluster_combo)
            .and_then(|t| t.split(' ').next().and_then(|n| n.parse::<u64>().ok()))
            .filter(|b| [512, 1024, 2048, 4096, 8192, 16384, 32768, 65536].contains(b))
    }

    fn static_win_options(me: &Rc<Inner>) -> WinOptions {
        WinOptions {
            hw_bypass: me.opt_hw.is_active(),
            no_online_account: me.opt_account.is_active(),
            no_bitlocker: me.opt_bitlocker.is_active(),
        }
    }

    /// The plan actually executed: the auto-detected base with the user's
    /// scheme / file-system / persistence / experience overrides layered on
    /// (Rufus-style).
    fn static_effective_plan(me: &Rc<Inner>) -> Option<FlashPlan> {
        let base = me.plan.borrow().clone()?;
        let scheme = Self::static_chosen_scheme(me);
        // "NTFS" explicitly chosen for a Windows ISO → single NTFS layout
        // with the uefi:ntfs loader instead of FAT32.
        let wants_ntfs = combo_text(&me.fs_combo).as_deref() == Some("NTFS");
        let options = Self::static_win_options(me);
        let persist_mb = me.persist_row.value() as u64;
        let persist_label = me.flavor.borrow().clone();

        Some(match base {
            FlashPlan::WinFat32 { image, .. } | FlashPlan::WinUefiNtfs { image, .. }
                if me.wtg_switch.is_active() =>
            {
                // Windows To Go boots UEFI only — force GPT regardless of
                // the scheme combo; the confirm dialog shows the layout.
                FlashPlan::WinToGo {
                    image,
                    wim_index: me.wtg_index.value() as u32,
                    scheme: FlashPlanScheme::Gpt,
                    options,
                }
            }
            FlashPlan::RawDd { image, verify, .. } => FlashPlan::RawDd {
                image,
                verify,
                persistence_mb: persist_mb,
                persistence_label: persist_label,
            },
            FlashPlan::WinFat32 { image, .. } if wants_ntfs => FlashPlan::WinUefiNtfs {
                image,
                scheme,
                options,
            },
            FlashPlan::WinFat32 { image, split_wim, .. } => FlashPlan::WinFat32 {
                image,
                split_wim,
                scheme,
                options,
            },
            FlashPlan::WinUefiNtfs { image, .. } => FlashPlan::WinUefiNtfs {
                image,
                scheme,
                options,
            },
            other => other,
        })
    }

    fn static_on_start(me: &Rc<Inner>) {
        if me.busy.get() {
            return;
        }
        let Some(dev) = Self::selected_device(me) else {
            return;
        };
        let non_boot = me.boot_combo.selected() == 0;

        let bad_blocks = Self::static_bad_blocks_choice(me);

        if non_boot {
            let fs = combo_text(&me.fs_combo).unwrap_or_else(|| "FAT32".into());
            let label = {
                let t = me.label_entry.text().trim().to_string();
                if t.is_empty() { "FERRUS".to_string() } else { t }
            };
            let plan = FlashPlan::FormatDevice {
                scheme: Self::static_chosen_scheme(me),
                fs,
                label,
                cluster_bytes: Self::static_cluster_choice(me),
                old_bios_align: me.bios_fixes.is_active(),
            };

            let heading = format!("Format {} ?", dev.devnode);
            let body = format!(
                "ALL data on {} ({}) will be destroyed.\n\n\
                 The stick will be partitioned and formatted as {} — no bootable image is written.",
                dev.devnode,
                dev.size_string(),
                plan.describe()
            );
            let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
            dialog.add_response("cancel", "_Cancel");
            dialog.add_response("erase", "_Format");
            dialog.set_response_appearance("erase", adw::ResponseAppearance::Destructive);
            dialog.set_default_response(Some("cancel"));
            dialog.set_close_response("cancel");

            let me2 = me.clone();
            glib::spawn_future_local(async move {
                if dialog.choose_future(&me2.window).await != "erase" {
                    return;
                }
                Self::static_begin_flash(&me2, &dev.devnode, plan, bad_blocks);
            });
            return;
        }

        let Some(image) = me.iso_path.borrow().clone() else {
            return;
        };
        let Some(plan) = Self::static_effective_plan(me) else {
            return;
        };

        let heading = format!("Erase {} ?", dev.devnode);
        let mut body = format!(
            "ALL data on {} ({}) will be destroyed.\n\n\
             Source image:\n{}\n\nLayout: {}",
            dev.devnode,
            dev.size_string(),
            image,
            plan.describe()
        );
        match &plan {
            FlashPlan::RawDd {
                persistence_mb, ..
            } if *persistence_mb > 0 => {
                let label = me.flavor.borrow().clone().unwrap_or_default();
                if label.is_empty() {
                    body.push_str(&format!("\nPersistence: {persistence_mb} MiB"));
                } else {
                    body.push_str(&format!(
                        "\nPersistence: {persistence_mb} MiB (label “{label}”)"
                    ));
                }
            }
            FlashPlan::WinFat32 { options, .. }
            | FlashPlan::WinUefiNtfs { options, .. }
            | FlashPlan::WinToGo { options, .. } => {
                let mut tweaks = Vec::new();
                if options.hw_bypass {
                    tweaks.push("no HW requirement checks");
                }
                if options.no_online_account {
                    tweaks.push("local account");
                }
                if options.no_bitlocker {
                    tweaks.push("BitLocker off");
                }
                if !tweaks.is_empty() {
                    body.push_str(&format!("\nUnattend: {}", tweaks.join(", ")));
                }
            }
            _ => {}
        }

        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        dialog.add_response("cancel", "_Cancel");
        dialog.add_response("erase", "_Erase");
        dialog.set_response_appearance("erase", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");

        let me2 = me.clone();
        glib::spawn_future_local(async move {
            let answer = dialog.choose_future(&me2.window).await;
            if answer != "erase" {
                return;
            }
            Self::static_begin_flash(&me2, &dev.devnode, plan, bad_blocks);
        });
    }

    fn static_begin_flash(
        me: &Rc<Inner>,
        devnode: &str,
        plan: FlashPlan,
        bad_blocks: Option<BadBlocks>,
    ) {
        let helper = match client::resolve_helper_path() {
            Some(p) => p,
            None => {
                me.status_label
                    .set_text("Error: ferrus-helper not found (set FERRUS_HELPER_PATH)");
                return;
            }
        };

        me.busy.set(true);
        Self::static_update_sensitivity(me);
        me.progress.set_visible(true);
        me.progress.set_fraction(0.0);
        me.status_label
            .set_text(&format!("Acquiring {devnode} exclusively…"));

        match client::spawn_flash_plan(&helper, devnode, plan, bad_blocks) {
            Ok((rx, handle)) => {
                *me.flash.borrow_mut() = Some(handle);
                let me2 = me.clone();
                glib::timeout_add_local(std::time::Duration::from_millis(60), move || loop {
                    match rx.try_recv() {
                        Ok(ev) => {
                            if Self::static_on_event(&me2, ev) == ControlFlow::Break {
                                return ControlFlow::Break;
                            }
                        }
                        Err(TryRecvError::Empty) => return ControlFlow::Continue,
                        Err(TryRecvError::Disconnected) => return ControlFlow::Break,
                    }
                });
            }
            Err(e) => {
                me.busy.set(false);
                Self::static_update_sensitivity(me);
                me.progress.set_visible(false);
                me.status_label
                    .set_text(&format!("Failed to start flashing: {e:#}"));
            }
        }
    }

    fn static_on_event(me: &Rc<Inner>, ev: ClientEvent) -> ControlFlow {
        match ev {
            ClientEvent::Progress {
                done,
                total,
                verifying,
                phase,
            } => {
                let phase_txt = phase.unwrap_or_else(|| {
                    if verifying {
                        "verifying".into()
                    } else {
                        "writing".into()
                    }
                });
                if total > 0 {
                    me.progress.set_fraction(done as f64 / total as f64);
                    me.progress
                        .set_text(Some(&format!("{phase_txt} · {} / {}", fmt_size(done), fmt_size(total))));
                } else {
                    me.progress.pulse();
                    me.progress.set_text(Some(&phase_txt));
                }
                ControlFlow::Continue
            }
            ClientEvent::Done(result) => {
                *me.flash.borrow_mut() = None;
                me.busy.set(false);
                Self::static_update_sensitivity(me);
                match result {
                    Ok(msg) => {
                        me.progress.set_fraction(1.0);
                        me.progress.set_text(Some("Done"));
                        me.status_label
                            .set_text(&format!("Success: {msg}. You can remove the drive."));
                    }
                    Err(e) => {
                        me.progress.set_visible(false);
                        me.status_label.set_text(&format!("Failed: {e}"));
                    }
                }
                ControlFlow::Continue
            }
        }
    }
}

fn set_combo_strings(combo: &ComboRow, items: &[&str]) {
    combo.set_model(Some(&StringList::new(items)));
}

fn combo_text(combo: &ComboRow) -> Option<String> {
    combo
        .selected_item()
        .and_downcast::<gtk::StringObject>()
        .map(|o| o.string().to_string())
}

pub(crate) fn fmt_size(bytes: u64) -> String {
    const GB: f64 = 1_000_000_000.0;
    const MB: f64 = 1_000_000.0;
    let gb = bytes as f64 / GB;
    if gb >= 1.0 {
        format!("{gb:.1} GB")
    } else {
        format!("{} MB", (bytes as f64 / MB).round() as u64)
    }
}

fn show_about(parent: &ApplicationWindow) {
    let about = adw::AboutDialog::new();
    about.set_application_name("Ferrus");
    about.set_developer_name("Ferrus contributors");
    about.set_version(env!("CARGO_PKG_VERSION"));
    about.set_comments("A native Linux bootable-USB creator with Rufus-grade feature coverage.");
    about.set_license_type(gtk::License::Gpl30Only);
    about.present(Some(parent));
}
