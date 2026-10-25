mod compression;

pub fn data_symbols() -> Vec<(String, u64)> {
    corefoundation::bootstrap();
    corefoundation::base::string_globals(compression::KEYS)
}

exports::exports! {}
