use std::path::Path;
use std::str::FromStr;

use anyhow::Result;
use log::{info, log_enabled, warn, Level};
use postgres::config::Config;
use postgres::{Client, NoTls};
use serde::Deserialize;

use crate::compute::ComputeNode;
use crate::config;
use crate::params::PG_HBA_ALL_MD5;
use crate::pg_helpers::*;

/// Cluster spec or configuration represented as an optional number of
/// delta operations + final cluster state description.
#[derive(Clone, Deserialize)]
pub struct ComputeSpec {
    pub format_version: f32,
    pub timestamp: String,
    pub operation_uuid: Option<String>,
    /// Expected cluster state at the end of transition process.
    pub cluster: Cluster,
    pub delta_operations: Option<Vec<DeltaOp>>,
}

/// Cluster state seen from the perspective of the external tools
/// like Rails web console.
#[derive(Clone, Deserialize)]
pub struct Cluster {
    pub cluster_id: String,
    pub name: String,
    pub state: Option<String>,
    pub roles: Vec<Role>,
    pub databases: Vec<Database>,
    pub settings: GenericOptions,
}

/// Single cluster state changing operation that could not be represented as
/// a static `Cluster` structure. For example:
/// - DROP DATABASE
/// - DROP ROLE
/// - ALTER ROLE name RENAME TO new_name
/// - ALTER DATABASE name RENAME TO new_name
#[derive(Clone, Deserialize)]
pub struct DeltaOp {
    pub action: String,
    pub name: PgIdent,
    pub new_name: Option<PgIdent>,
}

/// It takes cluster specification and does the following:
/// - Serialize cluster config and put it into `postgresql.conf` completely rewriting the file.
/// - Update `pg_hba.conf` to allow external connections.
pub fn handle_configuration(spec: &ComputeSpec, pgdata_path: &Path) -> Result<()> {
    // File `postgresql.conf` is no longer included into `basebackup`, so just
    // always write all config into it creating new file.
    config::write_postgres_conf(&pgdata_path.join("postgresql.conf"), spec)?;

    update_pg_hba(pgdata_path)?;

    Ok(())
}

/// Check `pg_hba.conf` and update if needed to allow external connections.
pub fn update_pg_hba(pgdata_path: &Path) -> Result<()> {
    // XXX: consider making it a part of spec.json
    info!("checking pg_hba.conf");
    let pghba_path = pgdata_path.join("pg_hba.conf");

    if config::line_in_file(&pghba_path, PG_HBA_ALL_MD5)? {
        info!("updated pg_hba.conf to allow external connections");
    } else {
        info!("pg_hba.conf is up-to-date");
    }

    Ok(())
}

/// Given a cluster spec json and open transaction it handles roles creation,
/// deletion and update.
pub fn handle_roles(spec: &ComputeSpec, client: &mut Client) -> Result<()> {
    let mut xact = client.transaction()?;
    let existing_roles: Vec<Role> = get_existing_roles(&mut xact)?;

    // Print a list of existing Postgres roles (only in debug mode)
    info!("postgres roles:");
    for r in &existing_roles {
        info_println!(
            "{} - {}:{}",
            " ".repeat(27 + 5),
            r.name,
            if r.encrypted_password.is_some() {
                "[FILTERED]"
            } else {
                "(null)"
            }
        );
    }

    // Process delta operations first
    if let Some(ops) = &spec.delta_operations {
        info!("processing role renames");
        for op in ops {
            match op.action.as_ref() {
                "delete_role" => {
                    // no-op now, roles will be deleted at the end of configuration
                }
                // Renaming role drops its password, since role name is
                // used as a salt there.  It is important that this role
                // is recorded with a new `name` in the `roles` list.
                // Follow up roles update will set the new password.
                "rename_role" => {
                    let new_name = op.new_name.as_ref().unwrap();

                    // XXX: with a limited number of roles it is fine, but consider making it a HashMap
                    if existing_roles.iter().any(|r| r.name == op.name) {
                        let query: String = format!(
                            "ALTER ROLE {} RENAME TO {}",
                            op.name.pg_quote(),
                            new_name.pg_quote()
                        );

                        warn!("renaming role '{}' to '{}'", op.name, new_name);
                        xact.execute(query.as_str(), &[])?;
                    }
                }
                _ => {}
            }
        }
    }

    // Refresh Postgres roles info to handle possible roles renaming
    let existing_roles: Vec<Role> = get_existing_roles(&mut xact)?;

    info!("cluster spec roles:");
    for role in &spec.cluster.roles {
        let name = &role.name;

        info_print!(
            "{} - {}:{}",
            " ".repeat(27 + 5),
            name,
            if role.encrypted_password.is_some() {
                "[FILTERED]"
            } else {
                "(null)"
            }
        );

        // XXX: with a limited number of roles it is fine, but consider making it a HashMap
        let pg_role = existing_roles.iter().find(|r| r.name == *name);

        if let Some(r) = pg_role {
            let mut update_role = false;

            if (r.encrypted_password.is_none() && role.encrypted_password.is_some())
                || (r.encrypted_password.is_some() && role.encrypted_password.is_none())
            {
                update_role = true;
            } else if let Some(pg_pwd) = &r.encrypted_password {
                // Check whether password changed or not (trim 'md5:' prefix first)
                update_role = pg_pwd[3..] != *role.encrypted_password.as_ref().unwrap();
            }

            if update_role {
                let mut query: String = format!("ALTER ROLE {} ", name.pg_quote());
                info_print!(" -> update");

                query.push_str(&role.to_pg_options());
                xact.execute(query.as_str(), &[])?;
            }
        } else {
            info!("role name: '{}'", &name);
            let mut query: String = format!("CREATE ROLE {} ", name.pg_quote());
            info!("role create query: '{}'", &query);
            info_print!(" -> create");

            query.push_str(&role.to_pg_options());
            xact.execute(query.as_str(), &[])?;

            let grant_query = format!(
                "GRANT pg_read_all_data, pg_write_all_data TO {}",
                name.pg_quote()
            );
            xact.execute(grant_query.as_str(), &[])?;
            info!("role grant query: '{}'", &grant_query);
        }

        info_print!("\n");
    }

    xact.commit()?;

    Ok(())
}

/// Reassign all dependent objects and delete requested roles.
pub fn handle_role_deletions(node: &ComputeNode, client: &mut Client) -> Result<()> {
    let spec = &node.spec;

    // First, reassign all dependent objects to db owners.
    if let Some(ops) = &spec.delta_operations {
        info!("reassigning dependent objects of to-be-deleted roles");
        for op in ops {
            if op.action == "delete_role" {
                reassign_owned_objects(node, &op.name)?;
            }
        }
    }

    // Second, proceed with role deletions.
    let mut xact = client.transaction()?;
    if let Some(ops) = &spec.delta_operations {
        info!("processing role deletions");
        for op in ops {
            // We do not check either role exists or not,
            // Postgres will take care of it for us
            if op.action == "delete_role" {
                let query: String = format!("DROP ROLE IF EXISTS {}", &op.name.pg_quote());

                warn!("deleting role '{}'", &op.name);
                xact.execute(query.as_str(), &[])?;
            }
        }
    }

    Ok(())
}

// Reassign all owned objects in all databases to the owner of the database.
fn reassign_owned_objects(node: &ComputeNode, role_name: &PgIdent) -> Result<()> {
    for db in &node.spec.cluster.databases {
        if db.owner != *role_name {
            let mut conf = Config::from_str(node.connstr.as_str())?;
            conf.dbname(&db.name);

            let mut client = conf.connect(NoTls)?;

            // This will reassign all dependent objects to the db owner
            let reassign_query = format!(
                "REASSIGN OWNED BY {} TO {}",
                role_name.pg_quote(),
                db.owner.pg_quote()
            );
            info!(
                "reassigning objects owned by '{}' in db '{}' to '{}'",
                role_name, &db.name, &db.owner
            );
            client.simple_query(&reassign_query)?;

            // This now will only drop privileges of the role
            let drop_query = format!("DROP OWNED BY {}", role_name.pg_quote());
            client.simple_query(&drop_query)?;
        }
    }

    Ok(())
}

/// It follows mostly the same logic as `handle_roles()` excepting that we
/// does not use an explicit transactions block, since major database operations
/// like `CREATE DATABASE` and `DROP DATABASE` do not support it. Statement-level
/// atomicity should be enough here due to the order of operations and various checks,
/// which together provide us idempotency.
pub fn handle_databases(spec: &ComputeSpec, client: &mut Client) -> Result<()> {
    let existing_dbs: Vec<Database> = get_existing_dbs(client)?;

    // Print a list of existing Postgres databases (only in debug mode)
    info!("postgres databases:");
    for r in &existing_dbs {
        info_println!("{} - {}:{}", " ".repeat(27 + 5), r.name, r.owner);
    }

    // Process delta operations first
    if let Some(ops) = &spec.delta_operations {
        info!("processing delta operations on databases");
        for op in ops {
            match op.action.as_ref() {
                // We do not check either DB exists or not,
                // Postgres will take care of it for us
                "delete_db" => {
                    let query: String = format!("DROP DATABASE IF EXISTS {}", &op.name.pg_quote());

                    warn!("deleting database '{}'", &op.name);
                    client.execute(query.as_str(), &[])?;
                }
                "rename_db" => {
                    let new_name = op.new_name.as_ref().unwrap();

                    // XXX: with a limited number of roles it is fine, but consider making it a HashMap
                    if existing_dbs.iter().any(|r| r.name == op.name) {
                        let query: String = format!(
                            "ALTER DATABASE {} RENAME TO {}",
                            op.name.pg_quote(),
                            new_name.pg_quote()
                        );

                        warn!("renaming database '{}' to '{}'", op.name, new_name);
                        client.execute(query.as_str(), &[])?;
                    }
                }
                _ => {}
            }
        }
    }

    // Refresh Postgres databases info to handle possible renames
    let existing_dbs: Vec<Database> = get_existing_dbs(client)?;

    info!("cluster spec databases:");
    for db in &spec.cluster.databases {
        let name = &db.name;

        info_print!("{} - {}:{}", " ".repeat(27 + 5), db.name, db.owner);

        // XXX: with a limited number of databases it is fine, but consider making it a HashMap
        let pg_db = existing_dbs.iter().find(|r| r.name == *name);

        if let Some(r) = pg_db {
            // XXX: db owner name is returned as quoted string from Postgres,
            // when quoting is needed.
            let new_owner = if r.owner.starts_with('"') {
                db.owner.pg_quote()
            } else {
                db.owner.clone()
            };

            if new_owner != r.owner {
                let query: String = format!(
                    "ALTER DATABASE {} OWNER TO {}",
                    name.pg_quote(),
                    db.owner.pg_quote()
                );
                info_print!(" -> update");

                client.execute(query.as_str(), &[])?;
            }
        } else {
            let mut query: String = format!("CREATE DATABASE {} ", name.pg_quote());
            info_print!(" -> create");

            query.push_str(&db.to_pg_options());
            client.execute(query.as_str(), &[])?;
        }

        info_print!("\n");
    }

    Ok(())
}

/// Grant CREATE ON DATABASE to the database owner and do some other alters and grants
/// to allow users creating trusted extensions and re-creating `public` schema, for example.
pub fn handle_grants(node: &ComputeNode, client: &mut Client) -> Result<()> {
    let spec = &node.spec;

    info!("cluster spec grants:");

    // We now have a separate `web_access` role to connect to the database
    // via the web interface and proxy link auth. And also we grant a
    // read / write all data privilege to every role. So also grant
    // create to everyone.
    // XXX: later we should stop messing with Postgres ACL in such horrible
    // ways.
    let roles = spec
        .cluster
        .roles
        .iter()
        .map(|r| r.name.pg_quote())
        .collect::<Vec<_>>();

    for db in &spec.cluster.databases {
        let dbname = &db.name;

        let query: String = format!(
            "GRANT CREATE ON DATABASE {} TO {}",
            dbname.pg_quote(),
            roles.join(", ")
        );
        info!("grant query {}", &query);

        client.execute(query.as_str(), &[])?;
    }

    // Do some per-database access adjustments. We'd better do this at db creation time,
    // but CREATE DATABASE isn't transactional. So we cannot create db + do some grants
    // atomically.
    for db in &node.spec.cluster.databases {
        let mut conf = Config::from_str(node.connstr.as_str())?;
        conf.dbname(&db.name);

        let mut db_client = conf.connect(NoTls)?;

        // This will only change ownership on the schema itself, not the objects
        // inside it. Without it owner of the `public` schema will be `cloud_admin`
        // and database owner cannot do anything with it. SQL procedure ensures
        // that it won't error out if schema `public` doesn't exist.
        let alter_query = format!(
            "DO $$\n\
                DECLARE\n\
                    schema_owner TEXT;\n\
                BEGIN\n\
                    IF EXISTS(\n\
                        SELECT nspname\n\
                        FROM pg_catalog.pg_namespace\n\
                        WHERE nspname = 'public'\n\
                    )\n\
                    THEN\n\
                        SELECT nspowner::regrole::text\n\
                            FROM pg_catalog.pg_namespace\n\
                            WHERE nspname = 'public'\n\
                            INTO schema_owner;\n\
                \n\
                        IF schema_owner = 'cloud_admin' OR schema_owner = 'zenith_admin'\n\
                        THEN\n\
                            ALTER SCHEMA public OWNER TO {};\n\
                        END IF;\n\
                    END IF;\n\
                END\n\
            $$;",
            db.owner.pg_quote()
        );
        db_client.simple_query(&alter_query)?;

        // Explicitly grant CREATE ON SCHEMA PUBLIC to the web_access user.
        // This is needed since postgres 15, where this privilege is removed by default.
        let grant_query: String = "GRANT CREATE ON SCHEMA public TO web_access".to_string();
        info!("grant query for db {} : {}", &db.name, &grant_query);
        db_client.simple_query(&grant_query)?;
    }

    Ok(())
}
