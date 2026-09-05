use slates_wire::Wire;

#[derive(Wire)]
struct Bad<T> {
  value: T,
}

fn main() {}
