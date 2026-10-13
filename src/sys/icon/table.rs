//! Every theme's icon. `build.rs` includes this file too, so it stands on its own.

/// One theme's icon: the logo under `assets` it is made from, the `.ico` written there, and the
/// resource id it is embedded under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IconEntry {
    pub(crate) logo: &'static str,
    pub(crate) ico: &'static str,
    pub(crate) id: u16,
}

pub(crate) const CLASSIC: IconEntry = IconEntry {
    logo: "logo/monochrome-owl.png",
    ico: "hindsight.ico",
    id: 1,
};
pub(crate) const EMBER: IconEntry = IconEntry {
    logo: "logo/ember-owl.png",
    ico: "hindsight-ember.ico",
    id: 2,
};
pub(crate) const SELENE: IconEntry = IconEntry {
    logo: "logo/selene-owl.png",
    ico: "hindsight-selene.ico",
    id: 3,
};
pub(crate) const NOVA: IconEntry = IconEntry {
    logo: "logo/nova-owl.png",
    ico: "hindsight-nova.ico",
    id: 4,
};

pub(crate) const ICONS: [IconEntry; 4] = [CLASSIC, EMBER, SELENE, NOVA];
