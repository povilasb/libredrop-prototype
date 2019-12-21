/// Prints given formatted string and prompts for input again.

#[macro_export]
macro_rules! out {
    ($($arg:tt)*) => ({
        use std::io::{self, Write};
        print!("\r");
        println!($($arg)*);
        print!("\r> ");
        unwrap!(io::stdout().flush());
    });
}
