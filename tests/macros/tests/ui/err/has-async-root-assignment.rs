use fastrace::trace;

#[trace(async_root = true)]
async fn f() {}

fn main() {}
