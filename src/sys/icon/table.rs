//! Every theme's icon, in the order the themes are declared: the theme's name in the settings,
//! the logo under `assets` it is made from, the `.ico` written there, and the resource id it is
//! embedded under. `build.rs` includes this file too, so it stands on its own.

pub(crate) const ICONS: [(&str, &str, &str, u16); 4] = [
    ("classic", "logo/monochrome-owl.png", "hindsight.ico", 1),
    ("ember", "logo/ember-owl.png", "hindsight-ember.ico", 2),
    ("selene", "logo/selene-owl.png", "hindsight-selene.ico", 3),
    ("nova", "logo/nova-owl.png", "hindsight-nova.ico", 4),
];
