use fastrace::trace;

#[trace(async_root)]
fn f() {}

fn main() {}
