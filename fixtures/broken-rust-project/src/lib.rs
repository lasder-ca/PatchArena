//! Deliberately incorrect crate used as a PatchArena example target.

/// Return the sum of two integers.
pub fn add(left: i32, right: i32) -> i32 {
    left - right
}

#[cfg(test)]
mod tests {
    #[test]
    fn adds_positive_integers() {
        assert_eq!(super::add(2, 3), 5);
    }
}
