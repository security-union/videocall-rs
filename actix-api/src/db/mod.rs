use postgres::NoTls;
use r2d2::Pool;
use r2d2::PooledConnection;
use r2d2_postgres::{postgres, PostgresConnectionManager};
use std::env;

pub type PostgresPool = Pool<PostgresConnectionManager<NoTls>>;
pub type PostgresConnection = PooledConnection<PostgresConnectionManager<NoTls>>;

pub fn get_database_url() -> String {
    env::var("PG_URL").unwrap()
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
