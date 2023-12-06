cargo_component_bindings::generate!();
use bindings::Guest;

struct Component;

impl Guest for Component {
    fn add(x: i32, y: i32) -> i32 {
        x + y
    }
}
