/// std::type_name: canonical type strings (resolved at compile time).
module demo::typename;

use std::ascii::String;
use std::type_name;

public struct Coin<phantom T> has drop {}

public struct USD has drop {}

public fun coin_name(): String { type_name::with_defining_ids<Coin<USD>>().into_string() }

public fun vector_name(): String { type_name::with_original_ids<vector<u64>>().into_string() }

public fun module_of(): String { type_name::with_defining_ids<USD>().module_string() }

public fun defining(): address { type_name::defining_id<Coin<u8>>() }

public fun primitive(): bool { type_name::with_defining_ids<u64>().is_primitive() }
