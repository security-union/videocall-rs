/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

use postgres::NoTls;
use r2d2::Pool;
use r2d2::PooledConnection;
use r2d2_postgres::{postgres, PostgresConnectionManager};
use std::env;

pub type PostgresPool = Pool<PostgresConnectionManager<NoTls>>;
pub type PostgresConnection = PooledConnection<PostgresConnectionManager<NoTls>>;

pub fn get_database_url() -> String {
    env::var("DATABASE_URL").unwrap()
}

pub fn get_pool() -> PostgresPool {
    let manager = PostgresConnectionManager::new(
        get_database_url()
            .parse()
            .expect("Database url is in a bad format."),
        NoTls,
    );
    Pool::builder()
        .max_size(5)
        .build(manager)
        .expect("Failed to build a database connection pool")
}

pub fn get_connection_query() -> Result<PostgresConnection, r2d2::Error> {
    get_pool().get()
}
