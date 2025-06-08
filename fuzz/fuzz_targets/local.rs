#![no_main]

libfuzzer_sys::fuzz_target!(
    init: {
        yclip::init_logging(yclip::tracing::Level::INFO).unwrap();
        yclip::test::test_setup();
    },
    |input: (String, String)| {
        let (input, password) = input;
        yclip::test::test(input.as_str(), password.as_str());
    }
);
