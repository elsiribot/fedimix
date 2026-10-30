//! About dialog.

use std::rc::Rc;

use adw::prelude::*;

use crate::ui::app_state::AppContext;

pub fn show(ctx: &Rc<AppContext>) {
    let dialog = adw::AboutDialog::builder()
        .application_name("Fedimix")
        .application_icon("io.sirion.Fedimix")
        .version(env!("CARGO_PKG_VERSION"))
        .comments("Private ecash spending with cold storage Saving.")
        // The repository has no license yet.
        .license_type(gtk::License::Unknown)
        .build();
    dialog.present(Some(&ctx.nav));
}
