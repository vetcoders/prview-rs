pub fn double(value: i32) -> i32 {
    value * 2
}

#[cfg(test)]
mod tests {
    /// The test a change-scoped run must NOT execute when only the root
    /// package changed.
    #[test]
    fn doubles_a_number() {
        assert_eq!(super::double(21), 42);
    }
}
