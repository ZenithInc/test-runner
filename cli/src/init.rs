use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::{EnvTemplate, InitArgs};
use crate::config::{TESTRUNNER_DIR, project_root};

pub async fn run(args: InitArgs) -> Result<()> {
    let root = args
        .root
        .canonicalize()
        .unwrap_or_else(|_| args.root.clone());
    if !root.exists() {
        bail!("target root {} does not exist", root.display());
    }

    let scaffold_root = project_root(&root);
    if scaffold_root.exists() && !args.force {
        bail!(
            "{} already exists under {} (pass --force to overwrite generated files)",
            TESTRUNNER_DIR,
            root.display()
        );
    }

    create_directories(&scaffold_root, args.with_mock)?;

    for template in templates(&root, &args.env_template, args.with_mock) {
        write_template(
            &scaffold_root,
            &template.path,
            &template.contents,
            args.force,
        )?;
    }

    println!("Initialized {} in {}", TESTRUNNER_DIR, root.display());
    println!("Next steps:");
    println!("  1. Update datasource URLs in .testrunner/datasources/*.yaml");
    println!("  2. Adjust env base URLs in .testrunner/env/*.yaml");
    println!("  3. Run `test-runner test all --dry-run` to inspect the discovered cases");
    Ok(())
}

#[derive(Debug, Clone)]
struct TemplateFile {
    path: PathBuf,
    contents: String,
}

fn create_directories(root: &Path, with_mock: bool) -> Result<()> {
    let mut directories = vec![
        root.to_path_buf(),
        root.join("env"),
        root.join("datasources"),
        root.join("apis").join("user"),
        root.join("cases").join("user").join("get-user"),
        root.join("cases").join("user").join("create-user"),
        root.join("data").join("common"),
        root.join("data").join("sql"),
        root.join("hooks").join("setup"),
        root.join("hooks").join("teardown"),
        root.join("reports"),
        root.join("workflows"),
    ];

    if with_mock {
        directories.push(root.join("mocks"));
        directories.push(root.join("mocks").join("routes"));
        directories.push(root.join("mocks").join("fixtures"));
    }

    for directory in directories {
        fs::create_dir_all(&directory)
            .with_context(|| format!("failed to create {}", directory.display()))?;
    }

    Ok(())
}

fn write_template(root: &Path, relative: &Path, contents: &str, force: bool) -> Result<()> {
    let destination = root.join(relative);
    if destination.exists() && !force {
        bail!(
            "refusing to overwrite {} without --force",
            destination.display()
        );
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(&destination, contents)
        .with_context(|| format!("failed to write {}", destination.display()))
}

fn templates(root: &Path, env_template: &EnvTemplate, with_mock: bool) -> Vec<TemplateFile> {
    let project_name = root
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("sample-http-service");
    let default_env = match env_template {
        EnvTemplate::Ci => "ci",
        _ => "local",
    };
    let local_base_url = "http://127.0.0.1:3000";
    let ci_base_url = match env_template {
        EnvTemplate::Ci => "http://app:3000",
        _ => "https://ci.example.internal",
    };

    let mut files = vec![
        TemplateFile {
            path: PathBuf::from("project.yaml"),
            contents: format!(
                "version: 1\nproject:\n  name: {project_name}\ndefaults:\n  env: {default_env}\n  execution_mode: serial\n  timeout_ms: 30000\nmock:\n  enabled: {}\n  host: 127.0.0.1\n  port: 18080\n",
                if with_mock { "true" } else { "false" }
            ),
        },
        TemplateFile {
            path: PathBuf::from("env/local.yaml"),
            contents: format!(
                "name: local\nbase_url: {local_base_url}\nheaders:\n  x-test-env: local\nvariables:\n  tenant: local\n  mock_base_url: http://127.0.0.1:18080\n"
            ),
        },
        TemplateFile {
            path: PathBuf::from("env/ci.yaml"),
            contents: format!(
                "name: ci\nbase_url: {ci_base_url}\nheaders:\n  x-test-env: ci\nvariables:\n  tenant: ci\n  mock_base_url: http://127.0.0.1:18080\n"
            ),
        },
        TemplateFile {
            path: PathBuf::from("datasources/mysql.yaml"),
            contents:
                "datasources:\n  mysql.main:\n    kind: mysql\n    url: mysql://root:password@127.0.0.1:3306/app\n"
                    .to_string(),
        },
        TemplateFile {
            path: PathBuf::from("datasources/postgres.yaml"),
            contents:
                "datasources:\n  postgres.analytics:\n    kind: postgres\n    url: postgres://postgres:password@127.0.0.1:5432/app\n"
                    .to_string(),
        },
        TemplateFile {
            path: PathBuf::from("datasources/redis.yaml"),
            contents:
                "datasources:\n  redis.cache:\n    kind: redis\n    url: redis://127.0.0.1:6379/0\n    key_prefix: test-runner\n"
                    .to_string(),
        },
        TemplateFile {
            path: PathBuf::from("apis/user/get-user.yaml"),
            contents:
                "name: Get user\nmethod: GET\npath: /users/{id}\nheaders:\n  accept: application/json\nquery: {}\n"
                    .to_string(),
        },
        TemplateFile {
            path: PathBuf::from("apis/user/create-user.yaml"),
            contents:
                "name: Create user\nmethod: POST\npath: /users\nheaders:\n  content-type: application/json\nquery: {}\nbody:\n  name: \"{{ candidate_name }}\"\n  email: \"{{ candidate_email }}\"\n"
                    .to_string(),
        },
        TemplateFile {
            path: PathBuf::from("cases/user/get-user/smoke.yaml"),
            contents:
                "name: get-user smoke\napi: user/get-user\ntags:\n  - smoke\nvars:\n  user_id: \"${data.common.users[0].id}\"\nsetup:\n  - use_data: common/users.json\n  - sql:\n      datasource: mysql.main\n      file: data/sql/seed.sql\nsteps:\n  - request:\n      api: user/get-user\n      path_params:\n        id: \"{{ user_id }}\"\n    extract:\n      status_code: response.status\n      user_name: response.json.name\n    assert:\n      - eq: [response.status, 200]\n      - not_empty: [response.json.id]\n      - eq: [response.headers.content-type, application/json]\n  - query_db:\n      datasource: mysql.main\n      sql: \"select id, status from users where id = '{{ user_id }}'\"\n    extract:\n      db_user_status: result.rows[0].status\n    assert:\n      - eq: [result.row_count, 1]\n      - eq: [db_user_status, active]\n  - query_redis:\n      datasource: redis.cache\n      command: GET\n      args:\n        - \"user:{{ user_id }}:profile\"\n    extract:\n      cached_profile: result.value\n    assert:\n      - not_empty: [cached_profile]\nteardown:\n  - sql:\n      datasource: mysql.main\n      file: data/sql/cleanup.sql\n"
                    .to_string(),
        },
        TemplateFile {
            path: PathBuf::from("cases/user/create-user/happy-path.yaml"),
            contents:
                "name: create-user happy path\napi: user/create-user\ntags:\n  - regression\nvars:\n  candidate_name: Alice Runner\n  candidate_email: alice.runner@example.com\nsteps:\n  - request:\n      api: user/create-user\n    extract:\n      created_id: response.json.id\n    assert:\n      - eq: [response.status, 201]\n      - not_empty: [created_id]\n"
                    .to_string(),
        },
        TemplateFile {
            path: PathBuf::from("data/common/users.json"),
            contents:
                "[\n  {\n    \"id\": \"u-001\",\n    \"name\": \"Alice Runner\",\n    \"email\": \"alice.runner@example.com\"\n  }\n]\n"
                    .to_string(),
        },
        TemplateFile {
            path: PathBuf::from("data/sql/seed.sql"),
            contents: "insert into users (id, name, status) values ('u-001', 'Alice Runner', 'active');\n"
                .to_string(),
        },
        TemplateFile {
            path: PathBuf::from("data/sql/cleanup.sql"),
            contents: "delete from users where id = 'u-001';\n".to_string(),
        },
        TemplateFile {
            path: PathBuf::from("workflows/get-user-flow.yaml"),
            contents: concat!(
                "name: sample workflow\n",
                "description: Demonstrates run_case sequencing and workflow branching.\n",
                "steps:\n",
                "  - run_case:\n",
                "      id: first-get-user\n",
                "      case: user/get-user/smoke\n",
                "      cleanup: immediate\n",
                "  - if: \"${workflow.steps.first-get-user.passed}\"\n",
                "    then:\n",
                "      - run_case:\n",
                "          id: second-get-user\n",
                "          case: user/get-user/smoke\n",
                "          cleanup: immediate\n",
                "    else:\n",
                "      - run_case:\n",
                "          id: fallback-create-user\n",
                "          case: user/create-user/happy-path\n",
                "          cleanup: immediate\n",
            )
            .to_string(),
        },
    ];

    if with_mock {
        files.push(TemplateFile {
            path: PathBuf::from("mocks/server.yaml"),
            contents: "enabled: true\nhost: 127.0.0.1\nport: 18080\n".to_string(),
        });
        files.push(TemplateFile {
            path: PathBuf::from("mocks/routes/user-profile.yaml"),
            contents:
                "method: GET\npath: /profiles/u-001\nstatus: 200\nheaders:\n  content-type: application/json\nbody_file: mocks/fixtures/user-profile.json\n"
                    .to_string(),
        });
        files.push(TemplateFile {
            path: PathBuf::from("mocks/fixtures/user-profile.json"),
            contents: "{\n  \"id\": \"u-001\",\n  \"display_name\": \"Alice Runner\"\n}\n"
                .to_string(),
        });
    }

    files
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn scaffold_is_created() {
        let temp = tempdir().expect("tempdir");
        run(InitArgs {
            root: temp.path().to_path_buf(),
            force: false,
            env_template: EnvTemplate::Local,
            with_mock: true,
        })
        .await
        .expect("init should succeed");

        assert!(temp.path().join(".testrunner/project.yaml").exists());
        assert!(
            temp.path()
                .join(".testrunner/cases/user/get-user/smoke.yaml")
                .exists()
        );
        assert!(
            temp.path()
                .join(".testrunner/mocks/routes/user-profile.yaml")
                .exists()
        );
        assert!(temp.path().join(".testrunner/workflows").exists());
        assert!(
            temp.path()
                .join(".testrunner/workflows/get-user-flow.yaml")
                .exists()
        );
    }
}
