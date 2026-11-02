use helix_core::{HelixDB, HelixDbSource};

fn main() {
    // Type-check direct engine access; no server and no inference are invoked.
    println!("{} / {}", std::any::type_name::<HelixDB>(), std::any::type_name::<HelixDbSource>());
}
