// App Volume Lock: keeps chosen apps at the volume you set.
//
// Some apps update themselves into a new folder, and Windows remembers per-app volume
// by the program's path, so after every update they start at 100 % again. This tray app
// matches apps by program name instead and puts their volume back, the same way the
// Volume Mixer would. Nothing else is touched.
#![windows_subsystem = "windows"]

mod audio;
mod gfx;
mod settings;
mod startup;
mod theme;
mod ui;

fn main() {
    ui::run();
}
