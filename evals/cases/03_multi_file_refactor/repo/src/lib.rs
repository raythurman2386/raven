mod helper;

pub fn greet(name: &str) -> String {
    format!("Hello, {name}!")
}

#[cfg(test)]
mod tests {
    use crate::helper::call_greet;

    #[test]
    fn greeting() {
        assert_eq!(call_greet("Ada"), "Hello, Ada!");
    }
}
