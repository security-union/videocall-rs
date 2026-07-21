# SQLite migrations

The SQLite equivalent of `dbmate/db/`, used when `meeting-api` is built with
`--no-default-features --features sqlite`.

The layout matches `dbmate/db/`: `db/migrations` and `db/schema.sql` are
dbmate's defaults, so no config file is needed.

Run dbmate from *this* directory, exactly as `dbmate/startup.sh` does for
PostgreSQL from `dbmate/`:

```bash
cd dbmate/sqlite
DATABASE_URL="sqlite:meeting-api.sqlite3" dbmate up
```

`dbmate/db/migrations` remains the source of truth for the schema. dbmate has no
dialect layer, so a new PostgreSQL migration needs a hand-written SQLite
counterpart here. `db/migrations/20260307000001_initial_schema.sql` documents the
deliberate differences between the two.
