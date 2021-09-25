use git2::build::RepoBuilder;
use git2::{
    Cred, Direction, Index, ObjectType, Oid, PushOptions, RemoteCallbacks, Repository, Signature,
};
use handlebars::Handlebars;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs::create_dir_all;
use std::path::{Path, PathBuf};
use tempfile::{tempdir, TempDir};

use crate::models::application::Application;
use crate::models::application_assignment::ApplicationAssignment;
use crate::utils::error::Error;

pub struct GitopsWorkflow {
    pub application_repo_url: String,
}

impl GitopsWorkflow {
    pub fn new(application_repo_url: &str) -> Result<GitopsWorkflow, Error> {
        return Ok(GitopsWorkflow {
            application_repo_url: application_repo_url.to_string(),
        });
    }

    fn get_auth_callback(&self) -> RemoteCallbacks {
        // Prepare callbacks.
        let mut callbacks = RemoteCallbacks::new();

        // TODO: Migrate to secrets
        callbacks.credentials(|_url, username_from_url, _allowed_types| {
            let home_dir = env::var("HOME").unwrap();
            let private_ssh_key_path = std::path::Path::new(&home_dir).join(".ssh/id_rsa");

            Cred::ssh_key(
                username_from_url.unwrap(),
                None,
                &private_ssh_key_path,
                None,
            )
        });

        callbacks
    }

    fn get_repo_builder(&self) -> RepoBuilder {
        let auth_callback = self.get_auth_callback();

        // Prepare fetch options.
        let mut fetch_options = git2::FetchOptions::new();
        fetch_options.remote_callbacks(auth_callback);

        // Prepare builder.
        let mut builder = git2::build::RepoBuilder::new();
        builder.fetch_options(fetch_options);

        builder
    }

    fn clone_deployment_repo(
        &self,
        application: &Application,
        deployment_temp_dir: &TempDir,
    ) -> Result<Repository, Error> {
        let repo_path = deployment_temp_dir.path().join("template");

        let mut repo_builder = self.get_repo_builder();

        match repo_builder.clone(&application.spec.templates.application.source, &repo_path) {
            Ok(repo) => Ok(repo),
            Err(err) => Err(Error::GitError { source: err }),
        }
    }

    fn clone_application_gitops_repo(
        &self,
        application_gitops_temp_dir: &TempDir,
    ) -> Result<Repository, Error> {
        let repo_path = application_gitops_temp_dir.path().join("gitops");

        let mut repo_builder = self.get_repo_builder();

        match repo_builder.clone(&self.application_repo_url, &repo_path) {
            Ok(repo) => Ok(repo),
            Err(err) => Err(Error::GitError { source: err }),
        }
    }

    fn render(
        &self,
        template_path: &Path,
        repo_root_path: &Path,
        root_relative_path: &Path,
        values: &HashMap<&str, &str>,
    ) -> Result<Vec<PathBuf>, Error> {
        let mut paths = Vec::new();

        let output_path = repo_root_path.join(root_relative_path);
        create_dir_all(&output_path)?;

        let entries = std::fs::read_dir(template_path)?;

        for entry_result in entries {
            let entry = entry_result?;
            let file_type = entry.file_type()?;
            let is_dotted_file_name =
                entry.file_name().to_str().unwrap().chars().next().unwrap() == '.';

            let entry_template_path = entry.path();

            let output_relative_path = Path::new(root_relative_path).join(entry.file_name());
            let output_absolute_path = Path::new(&repo_root_path).join(&output_relative_path);

            if file_type.is_dir() {
                if !is_dotted_file_name {
                    let mut subpaths = self.render(
                        &entry_template_path,
                        repo_root_path,
                        &output_relative_path,
                        values,
                    )?;
                    paths.append(&mut subpaths);
                }
            } else {
                // println!("adding path to list {:?}", output_relative_path);
                paths.push(output_relative_path);

                let template = std::fs::read_to_string(entry_template_path)?;
                let mut handlebars = Handlebars::new();
                handlebars
                    .register_template_string("template", template)
                    .unwrap();

                let rendered_file = match handlebars.render("template", values) {
                    Ok(rendered_file) => rendered_file,
                    Err(err) => return Err(Error::RenderError { source: err }),
                };

                std::fs::write(output_absolute_path, rendered_file.as_bytes())?;
            }
        }

        Ok(paths)
    }

    fn link(&self, cluster_path: &Path) -> Result<(), Error> {
        // read all directory names (aka applications)
        let entries = std::fs::read_dir(cluster_path)?;

        let mut applications = vec![OsString::from("../common")];

        for entry_result in entries {
            let entry = entry_result?;
            let file_type = entry.file_type()?;

            let file_name = entry.file_name();

            if file_type.is_dir() && !file_name.eq("flux-system") {
                applications.push(file_name);
            }
        }

        let application_list: String = applications
            .into_iter()
            .map(|application| {
                let display_string = application.to_string_lossy();
                format!("    - {}\n", display_string)
            })
            .collect();
        let kustomization = format!(
            "apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
{}",
            application_list
        );

        let kustomization_path = cluster_path.join("kustomization.yaml");

        std::fs::write(kustomization_path, kustomization.as_bytes())?;

        Ok(())
    }

    fn commit_files(
        &self,
        repo: &Repository,
        index: &mut Index,
        paths: Vec<PathBuf>,
        message: &str,
    ) -> Result<Oid, Error> {
        for path in paths.iter() {
            // println!("adding path to index {:?}", path);
            index.add_path(path)?;
        }

        let oid = index.write_tree()?;

        // TODO: Add mechanism to provide identity of commits.
        let signature = Signature::now("Application API", "application-api@example.com")?;

        let obj = repo.head()?.resolve()?.peel(ObjectType::Commit)?;
        let parent_commit = match obj.into_commit() {
            Ok(commit) => commit,
            Err(_) => {
                return Err(Error::GitError {
                    source: git2::Error::from_str("Couldn't find commit"),
                })
            }
        };

        let tree = repo.find_tree(oid)?;

        repo.commit(
            Some("HEAD"), //  point HEAD to our new commit
            &signature,   // author
            &signature,   // committer
            message,      // commit message
            &tree,        // tree
            &[&parent_commit],
        )?; // parents

        Ok(oid)
    }

    fn push(&self, repo: &Repository, url: &str, branch: &str) -> Result<(), Error> {
        let mut remote = match repo.find_remote("origin") {
            Ok(r) => r,
            Err(_) => repo.remote("origin", url)?,
        };

        let connect_auth_callback = self.get_auth_callback();
        remote.connect_auth(Direction::Push, Some(connect_auth_callback), None)?;

        let ref_spec = format!("refs/heads/{}:refs/heads/{}", branch, branch);

        let push_auth_callback = self.get_auth_callback();
        let mut push_options = PushOptions::new();
        push_options.remote_callbacks(push_auth_callback);

        remote.push(&[ref_spec], Some(&mut push_options))?;

        Ok(())
    }

    pub fn create_deployment(
        &self,
        application: &Application,
        application_assignment: &ApplicationAssignment,
    ) -> Result<Oid, Error> {
        let deployment_temp_dir = tempdir()?;
        let application_gitops_temp_dir = tempdir()?;

        // clone app repo specified by application.spec.templates.deployment.source
        let application_template_repo =
            self.clone_deployment_repo(application, &deployment_temp_dir)?;

        // clone application cluster gitops repo specified by application_repo_url
        let application_gitops_repo =
            self.clone_application_gitops_repo(&application_gitops_temp_dir)?;

        let template_path = Path::new(application_template_repo.path())
            .parent()
            .unwrap()
            .join(&application.spec.templates.application.path);

        let application_gitops_repo_path =
            Path::new(application_gitops_repo.path()).parent().unwrap();

        // TODO: should we be less opinionated / more configurable about where applications go?
        let cluster_relative_path = Path::new(&application_assignment.spec.cluster);
        let cluster_path = application_gitops_repo_path.join(&cluster_relative_path);

        let output_relative_path =
            cluster_relative_path.join(&application_assignment.spec.application);

        let mut index = application_gitops_repo.index()?;
        index.remove_dir(&output_relative_path, 0)?;

        // build template context variables
        let mut template_values: HashMap<&str, &str> = HashMap::new();
        template_values.insert("clusterName", &application_assignment.spec.cluster);

        // TODO: Fetch assigned cluster when we are using Cluster API
        template_values.insert("cloud", "azure");
        template_values.insert("cloudRegion", "eastus2");

        // add in values from Application
        if let Some(values) = &application.spec.values {
            for (key, value) in values.iter() {
                template_values.insert(&key, &value);
            }
        }

        let mut paths = self.render(
            &template_path,
            application_gitops_repo_path,
            &output_relative_path,
            &template_values,
        )?;
        self.link(&cluster_path)?;

        let kustomization_path = cluster_relative_path.join("kustomization.yaml");
        paths.push(kustomization_path);

        // TODO(ENH): Support different messages
        let message = format!(
            "Reconciling created ApplicationAssignment {} for Application {} for Cluster {}",
            application_assignment.metadata.name.as_ref().unwrap(),
            application_assignment.spec.application,
            application_assignment.spec.cluster
        );

        // add and commit output path in application cluster gitops repo
        let oid = self.commit_files(&application_gitops_repo, &mut index, paths, &message)?;

        // TODO: make more flexible to support different branches
        self.push(&application_gitops_repo, &self.application_repo_url, "main")?;

        Ok(oid)
    }

    pub fn delete_deployment(
        &self,
        application_assignment: &ApplicationAssignment,
    ) -> Result<Oid, Error> {
        println!("gitopsworkflow: delete_deployment");
        let application_gitops_temp_dir = tempdir()?;

        // clone application cluster gitops repo specified by application_repo_url
        let application_gitops_repo =
            self.clone_application_gitops_repo(&application_gitops_temp_dir)?;
        let application_gitops_repo_path =
            Path::new(application_gitops_repo.path()).parent().unwrap();

        let cluster_relative_path = Path::new(&application_assignment.spec.cluster);
        let cluster_path = application_gitops_repo_path.join(&cluster_relative_path);

        let output_relative_path =
            cluster_relative_path.join(&application_assignment.spec.application);

        let mut index = application_gitops_repo.index()?;

        index.remove_dir(&output_relative_path, 0)?;

        self.link(&cluster_path)?;

        let kustomization_path = cluster_relative_path.join("kustomization.yaml");
        let paths: Vec<PathBuf> = vec![kustomization_path];

        // TODO(ENH): Support different messages
        let message = format!(
            "Reconciling deleted ApplicationAssignment {} for Application {} for Cluster {}",
            application_assignment.metadata.name.as_ref().unwrap(),
            application_assignment.spec.application,
            application_assignment.spec.cluster
        );

        // add and commit output path in application cluster gitops repo
        let oid = self.commit_files(&application_gitops_repo, &mut index, paths, &message)?;

        // TODO: make more flexible to support different branches
        self.push(&application_gitops_repo, &self.application_repo_url, "main")?;

        Ok(oid)
    }
}

#[cfg(test)]
mod tests {
    use kube::core::metadata::ObjectMeta;
    use std::collections::HashMap;
    use std::path::Path;

    use crate::models::application::{Application, ApplicationSpec};
    use crate::models::application_assignment::{ApplicationAssignment, ApplicationAssignmentSpec};
    use crate::models::template_spec::TemplateSpec;
    use crate::models::templates_spec::TemplatesSpec;

    use super::GitopsWorkflow;

    #[test]
    fn can_create_deployment() {
        let workflow =
            GitopsWorkflow::new("git@github.com:timfpark/workload-cluster-gitops").unwrap();

        let mut values: HashMap<String, String> = HashMap::new();
        values.insert("ring".to_string(), "main".to_string());

        let application = Application {
            api_version: "v1".to_string(),
            kind: "Application".to_string(),
            metadata: ObjectMeta {
                name: Some("cluster-agent".to_string()),
                namespace: Some("default".to_string()),
                ..ObjectMeta::default()
            },
            spec: ApplicationSpec {
                templates: TemplatesSpec {
                    application: TemplateSpec {
                        method: Some("git".to_string()),
                        source: "git@github.com:timfpark/cluster-agent".to_string(),
                        path: "templates/deployment".to_string(),
                    },

                    global: None,
                },
                values: Some(values),
            },
        };

        let application_assignment = ApplicationAssignment {
            api_version: "v1".to_string(),
            kind: "ApplicationAssignment".to_string(),
            metadata: ObjectMeta {
                name: Some("azure-eastus2-1-cluster-agent".to_string()),
                namespace: Some("default".to_string()),
                ..ObjectMeta::default()
            },
            spec: ApplicationAssignmentSpec {
                cluster: "azure-eastus2-1".to_string(),
                application: "cluster-agent".to_string(),
            },
        };

        match workflow.create_deployment(&application, &application_assignment) {
            Err(err) => {
                eprintln!("create deployment failed with: {:?}", err);
                assert_eq!(false, true);
            }
            Ok(_) => {}
        }
    }

    #[test]
    fn can_render_application() {
        let workflow =
            GitopsWorkflow::new("git@github.com:timfpark/workload-cluster-gitops").unwrap();

        let mut values: HashMap<&str, &str> = HashMap::new();
        values.insert("CLUSTER_NAME", "my-cluster");

        let template_path = Path::new("./fixtures/template");
        let repo_root_path = Path::new("./fixtures/");
        let root_relative_path = Path::new("applications/my-cluster");
        let output_path = repo_root_path.join(root_relative_path);

        std::fs::create_dir_all(output_path).unwrap();

        let paths = workflow
            .render(template_path, repo_root_path, root_relative_path, &values)
            .unwrap();

        assert_eq!(paths.len(), 2);
    }
}
