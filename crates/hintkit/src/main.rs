fn main() {
    println!("hintkit v{}", env!("CARGO_PKG_VERSION"));
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_matches_phase_zero_stub() {
        assert_eq!(env!("CARGO_PKG_VERSION"), "0.0.0");
    }
}
