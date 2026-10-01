//! Dialogs for connecting to network shares and cloud drives.
//!
//! Both follow the same rule as the download dialog: whatever can be checked
//! before committing is checked here, so a bad address is a message under the
//! field rather than a failed mount several seconds later.

use std::{cell::RefCell, rc::Rc};

use adw::prelude::*;
use gtk::glib;

use crate::fs::{
    cloud::{NewAccount, Provider},
    remote::{Scheme, Server},
};

/// Asks for a server address.
///
/// `initial` pre-fills the fields, which is what makes "Edit" on a saved
/// connection work without a second dialog.
pub async fn ask_server(
    parent: &impl IsA<gtk::Widget>,
    initial: Option<Server>,
) -> Option<Server> {
    let schemes: Vec<Scheme> = Scheme::ALL.to_vec();
    let names: Vec<String> = schemes
        .iter()
        .map(|scheme| {
            if scheme.is_available() {
                scheme.label().to_string()
            } else {
                // Still listed, because hiding it leaves the user wondering why
                // their NAS is missing; the suffix says what to do about it.
                format!("{} — not installed", scheme.label())
            }
        })
        .collect();
    let model = gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>());

    let protocol = adw::ComboRow::builder().title("Protocol").model(&model).build();
    let selected = initial
        .as_ref()
        .and_then(|server| schemes.iter().position(|s| *s == server.scheme))
        .unwrap_or(0);
    protocol.set_selected(selected as u32);

    let address = adw::EntryRow::builder().title("Address").build();
    let user = adw::EntryRow::builder().title("Username").build();
    let label = adw::EntryRow::builder().title("Name in the sidebar").build();

    if let Some(server) = &initial {
        let mut text = server.host.clone();
        if let Some(port) = server.port {
            text.push_str(&format!(":{port}"));
        }
        if !server.path.is_empty() {
            text.push('/');
            text.push_str(&server.path);
        }
        address.set_text(&text);
        user.set_text(server.user.as_deref().unwrap_or(""));
        label.set_text(server.label.as_deref().unwrap_or(""));
    }

    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(52)
        .css_classes(["caption", "dim-label"])
        .build();

    let group = adw::PreferencesGroup::new();
    group.add(&protocol);
    group.add(&address);
    group.add(&user);
    group.add(&label);

    let cancel = gtk::Button::with_label("Cancel");
    let accept = gtk::Button::builder()
        .label("Connect")
        .css_classes(["suggested-action"])
        .sensitive(false)
        .build();

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(14)
        .margin_start(14)
        .margin_end(14)
        .build();
    body.append(&group);
    body.append(&status);

    let title = if initial.is_some() { "Edit connection" } else { "Connect to Server" };
    let header = adw::HeaderBar::builder().show_end_title_buttons(false).build();
    header.set_title_widget(Some(&adw::WindowTitle::new(title, "")));
    header.pack_start(&cancel);
    header.pack_end(&accept);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(500)
        .child(&toolbar)
        .build();

    let parsed: Rc<RefCell<Option<Server>>> = Rc::new(RefCell::new(None));

    // Re-validate whenever anything that feeds the address changes.
    let revalidate = {
        let (address, user, label, protocol, status, accept) = (
            address.clone(),
            user.clone(),
            label.clone(),
            protocol.clone(),
            status.clone(),
            accept.clone(),
        );
        let parsed = Rc::clone(&parsed);
        let schemes = schemes.clone();
        Rc::new(move || {
            let scheme = schemes
                .get(protocol.selected() as usize)
                .copied()
                .unwrap_or(Scheme::Smb);
            let text = address.text().to_string();

            if !scheme.is_available() {
                parsed.replace(None);
                accept.set_sensitive(false);
                status.add_css_class("error");
                status.set_label(&format!(
                    "{} needs a gvfs backend that is not installed.\nInstall {}.",
                    scheme.label(),
                    scheme.package_hint()
                ));
                return;
            }

            match crate::fs::remote::parse_address(&text, scheme) {
                Ok(mut server) => {
                    // The dedicated fields win over anything embedded in the
                    // address, because that is where the user last looked.
                    let typed_user = user.text().to_string();
                    if !typed_user.trim().is_empty() {
                        server.user = Some(typed_user.trim().to_string());
                    }
                    let typed_label = label.text().to_string();
                    if !typed_label.trim().is_empty() {
                        server.label = Some(typed_label.trim().to_string());
                    }
                    status.remove_css_class("error");
                    status.set_label(&format!("Will connect to {}", server.uri()));
                    accept.set_sensitive(true);
                    parsed.replace(Some(server));
                }
                Err(message) => {
                    parsed.replace(None);
                    accept.set_sensitive(false);
                    // An empty field is not a mistake the user has made yet.
                    if text.trim().is_empty() {
                        status.remove_css_class("error");
                        status.set_label("Enter the server's address");
                    } else {
                        status.add_css_class("error");
                        status.set_label(&message);
                    }
                }
            }
        })
    };

    for entry in [&address, &user, &label] {
        let revalidate = Rc::clone(&revalidate);
        entry.connect_changed(move |_| revalidate());
    }
    {
        let revalidate = Rc::clone(&revalidate);
        protocol.connect_selected_notify(move |_| revalidate());
    }
    revalidate();

    let confirmed = run(&dialog, parent, &accept, &cancel, &address).await;
    confirmed.then(|| parsed.borrow().clone()).flatten()
}

/// Asks which cloud account to add and how to sign in to it.
pub async fn ask_cloud(parent: &impl IsA<gtk::Widget>) -> Option<NewAccount> {
    if !crate::fs::cloud::is_available() {
        crate::ui::dialogs::show_error(
            parent,
            "rclone is not installed",
            crate::fs::cloud::INSTALL_HINT,
        );
        return None;
    }

    let providers = Provider::OFFERED.to_vec();
    let names: Vec<&str> = providers.iter().map(|p| p.label()).collect();
    let provider = adw::ComboRow::builder()
        .title("Provider")
        .model(&gtk::StringList::new(&names))
        .build();

    let name = adw::EntryRow::builder().title("Name").build();
    let username = adw::EntryRow::builder().title("Email or username").build();
    let password = adw::PasswordEntryRow::builder().title("Password").build();
    let totp = adw::EntryRow::builder().title("Two-factor code").build();
    let url = adw::EntryRow::builder().title("WebDAV address").build();
    let client_id = adw::EntryRow::builder().title("Client ID").build();
    let client_secret = adw::PasswordEntryRow::builder().title("Client secret").build();

    let group = adw::PreferencesGroup::new();
    for row in [
        provider.clone().upcast::<gtk::Widget>(),
        name.clone().upcast(),
        username.clone().upcast(),
        password.clone().upcast(),
        totp.clone().upcast(),
        url.clone().upcast(),
        client_id.clone().upcast(),
        client_secret.clone().upcast(),
    ] {
        group.add(&row);
    }
    // Named so the visibility rules below read as rules rather than indices.
    let (provider_row, name_row) = (provider.clone(), name.clone());
    let (username_row, password_row) = (username.clone(), password.clone());
    let (totp_row, url_row) = (totp.clone(), url.clone());
    let (client_id_row, client_secret_row) = (client_id.clone(), client_secret.clone());
    let _ = (&provider_row, &name_row);

    let status = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .max_width_chars(52)
        .css_classes(["caption", "dim-label"])
        .build();

    let cancel = gtk::Button::with_label("Cancel");
    let accept = gtk::Button::builder()
        .label("Connect")
        .css_classes(["suggested-action"])
        .build();

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .margin_top(12)
        .margin_bottom(14)
        .margin_start(14)
        .margin_end(14)
        .build();
    body.append(&group);
    body.append(&status);

    let header = adw::HeaderBar::builder().show_end_title_buttons(false).build();
    header.set_title_widget(Some(&adw::WindowTitle::new("Add a Cloud Drive", "")));
    header.pack_start(&cancel);
    header.pack_end(&accept);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&body));
    let dialog = adw::Dialog::builder()
        .title("Add a cloud drive")
        .content_width(500)
        .child(&toolbar)
        .build();

    // Only the fields the chosen provider actually uses are shown; an OAuth
    // provider showing an inert password box invites the user to type their
    // password into nothing.
    let update_fields = {
        let (username_row, password_row, totp_row, url_row, status, accept) = (
            username_row.clone(),
            password_row.clone(),
            totp_row.clone(),
            url_row.clone(),
            status.clone(),
            accept.clone(),
        );
        let (client_id_row, client_secret_row) =
            (client_id_row.clone(), client_secret_row.clone());
        let providers = providers.clone();
        let provider_widget = provider.clone();
        Rc::new(move || {
            let chosen = providers
                .get(provider_widget.selected() as usize)
                .copied()
                .unwrap_or(Provider::GoogleDrive);
            let oauth = chosen.uses_oauth();
            username_row.set_visible(!oauth);
            password_row.set_visible(!oauth);
            totp_row.set_visible(chosen == Provider::ProtonDrive);
            url_row.set_visible(chosen == Provider::Nextcloud || chosen == Provider::Other);
            client_id_row.set_visible(oauth);
            client_secret_row.set_visible(oauth);
            accept.set_label(if oauth { "Sign in with browser" } else { "Connect" });
            status.remove_css_class("error");
            status.set_label(if oauth {
                "A browser window will open so you can sign in. Fileman never sees the \
                 password. Google Drive does not report which account signed in, so the \
                 name above is what tells two of them apart — your email works well."
            } else {
                "The password is stored by rclone, not by Fileman."
            });
        })
    };
    {
        let update_fields = Rc::clone(&update_fields);
        provider.connect_selected_notify(move |_| update_fields());
    }
    update_fields();

    // An unnamed account is indistinguishable from the next one of the same
    // provider, so there is nothing useful to create without a name.
    {
        let accept = accept.clone();
        let set = move |entry: &adw::EntryRow| {
            accept.set_sensitive(!entry.text().trim().is_empty());
        };
        set(&name);
        name.connect_changed(move |entry| set(entry));
    }

    let confirmed = run(&dialog, parent, &accept, &cancel, &name).await;
    if !confirmed {
        return None;
    }

    let chosen = providers.get(provider.selected() as usize).copied()?;
    Some(NewAccount {
        provider: chosen,
        name: name.text().to_string(),
        username: username.text().to_string(),
        password: password.text().to_string(),
        totp: totp.text().to_string(),
        url: url.text().to_string(),
        client_id: client_id.text().to_string(),
        client_secret: client_secret.text().to_string(),
    })
}

/// Presents a dialog and resolves to whether it was accepted.
///
/// Shared because both dialogs need identical wiring, and duplicating the
/// close-counts-as-cancel handling is exactly how one of them ends up hanging.
async fn run(
    dialog: &adw::Dialog,
    parent: &impl IsA<gtk::Widget>,
    accept: &gtk::Button,
    cancel: &gtk::Button,
    focus: &impl IsA<gtk::Widget>,
) -> bool {
    let (tx, rx) = async_channel::bounded::<bool>(1);
    {
        let (tx, dialog) = (tx.clone(), dialog.clone());
        accept.connect_clicked(move |_| {
            let _ = tx.send_blocking(true);
            dialog.close();
        });
    }
    {
        let (tx, dialog) = (tx.clone(), dialog.clone());
        cancel.connect_clicked(move |_| {
            let _ = tx.send_blocking(false);
            dialog.close();
        });
    }
    dialog.connect_closed(move |_| {
        let _ = tx.try_send(false);
    });

    let focus = focus.clone();
    glib::idle_add_local_once(move || {
        focus.grab_focus();
    });

    dialog.present(Some(parent));
    rx.recv().await.unwrap_or(false)
}
