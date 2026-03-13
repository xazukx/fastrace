use fastrace::trace;

#[trace(async_root, enter_on_poll = true)]
async fn f() {}

fn main() {}
