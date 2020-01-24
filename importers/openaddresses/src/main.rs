use std::env;

fn main() {
    let args = env::args().collect::<Vec<String>>();
    if args.len() < 2 {
        eprintln!("Expected openaddresses folder");
        return;
    }

    let db = openaddresses::import_addresses("addresses.db", &args[1], true);

    println!(
        "Got {} addresses in {} cities (and {} errors)",
        db.get_nb_addresses(),
        db.get_nb_cities(),
        db.get_nb_errors(),
    );
    println!("Errors by categories:");
    let rows = db.get_nb_by_errors_kind();
    for (kind, nb) in rows {
        println!("  {} => {} occurences", kind, nb);
    }
}