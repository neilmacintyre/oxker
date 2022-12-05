use bollard::{
    container::{ListContainersOptions, LogsOptions, StartContainerOptions, Stats, StatsOptions},
    service::ContainerSummary,
    Docker,
};
use futures_util::{Future, StreamExt};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{sync::mpsc::Receiver, task::JoinHandle};
use uuid::Uuid;

use crate::{
    app_data::{AppData, ContainerId, DockerControls},
    app_error::AppError,
    parse_args::CliArgs,
    ui::{GuiState, Status}, ENTRY_POINT,
};
mod message;
pub use message::DockerMessage;

#[derive(Debug, Clone, Eq, Hash, PartialEq)]
enum SpawnId {
    Stats((ContainerId, Binate)),
    Log(ContainerId),
}

/// Cpu & Mem stats take twice as long as the update interval to get a value, so will have two being executed at the same time
/// SpawnId::Stats takes container_id and binate value to enable both cycles of the same container_id to be inserted into the hashmap
/// Binate value is toggled when all join handles have been spawned off
/// Also effectively means that if the docker_update interval minimum will be 1000ms
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq)]
enum Binate {
    One,
    Two,
}

impl Binate {
    const fn toggle(self) -> Self {
        match self {
            Self::One => Self::Two,
            Self::Two => Self::One,
        }
    }
}

pub struct DockerData {
    app_data: Arc<Mutex<AppData>>,
    args: CliArgs,
    binate: Binate,
    docker: Arc<Docker>,
    gui_state: Arc<Mutex<GuiState>>,
    initialised: bool,
    is_running: Arc<AtomicBool>,
    receiver: Receiver<DockerMessage>,
    spawns: Arc<Mutex<HashMap<SpawnId, JoinHandle<()>>>>,
}

impl DockerData {
    /// Use docker stats to caluclate current cpu usage
    #[allow(clippy::cast_precision_loss)]
    fn calculate_usage(stats: &Stats) -> f64 {
        let mut cpu_percentage = 0.0;
        let previous_cpu = stats.precpu_stats.cpu_usage.total_usage;
        let cpu_delta = stats.cpu_stats.cpu_usage.total_usage as f64 - previous_cpu as f64;

        if let (Some(cpu_stats_usage), Some(precpu_stats_usage)) = (
            stats.cpu_stats.system_cpu_usage,
            stats.precpu_stats.system_cpu_usage,
        ) {
            let system_delta = (cpu_stats_usage - precpu_stats_usage) as f64;
            let online_cpus = stats.cpu_stats.online_cpus.unwrap_or_else(|| {
                stats
                    .cpu_stats
                    .cpu_usage
                    .percpu_usage
                    .as_ref()
                    .map_or(0, std::vec::Vec::len) as u64
            }) as f64;
            if system_delta > 0.0 && cpu_delta > 0.0 {
                cpu_percentage = (cpu_delta / system_delta) * online_cpus * 100.0;
            }
        }
        cpu_percentage
    }

    /// Get a single docker stat in order to update mem and cpu usage
    /// don't take &self, so that can tokio::spawn into it's own thread
    /// remove if from spawns hashmap when complete
    async fn update_container_stat(
        docker: Arc<Docker>,
        id: ContainerId,
        app_data: Arc<Mutex<AppData>>,
        is_running: bool,
        spawns: Arc<Mutex<HashMap<SpawnId, JoinHandle<()>>>>,
        spawn_id: SpawnId,
    ) {
        let mut stream = docker
            .stats(
                id.get(),
                Some(StatsOptions {
                    stream: false,
                    one_shot: !is_running,
                }),
            )
            .take(1);

        while let Some(Ok(stats)) = stream.next().await {
            let mem_stat = stats.memory_stats.usage.unwrap_or(0);
            let mem_limit = stats.memory_stats.limit.unwrap_or(0);

            let op_key = stats
                .networks
                .as_ref()
                .and_then(|networks| networks.keys().next().cloned());

            let cpu_stats = Self::calculate_usage(&stats);

            let (rx, tx) = if let Some(key) = op_key {
                stats
                    .networks
                    .unwrap_or_default()
                    .get(&key)
                    .map_or((0, 0), |f| (f.rx_bytes, f.tx_bytes))
            } else {
                (0, 0)
            };

            if is_running {
                app_data.lock().update_stats(
                    &id,
                    Some(cpu_stats),
                    Some(mem_stat),
                    mem_limit,
                    rx,
                    tx,
                );
            } else {
                app_data
                    .lock()
                    .update_stats(&id, None, None, mem_limit, rx, tx);
            }
            spawns.lock().remove(&spawn_id);
        }
    }

    /// Update all stats, spawn each container into own tokio::spawn thread
    fn update_all_container_stats(&mut self, all_ids: &[(bool, ContainerId)]) {
        for (is_running, id) in all_ids {
            let docker = Arc::clone(&self.docker);
            let app_data = Arc::clone(&self.app_data);
            let spawns = Arc::clone(&self.spawns);
            let key = SpawnId::Stats((id.clone(), self.binate));
            let spawn_key = key.clone();
            self.spawns.lock().entry(key).or_insert_with(|| {
                tokio::spawn(Self::update_container_stat(
                    docker,
                    id.clone(),
                    app_data,
                    *is_running,
                    spawns,
                    spawn_key,
                ))
            });
        }
        self.binate = self.binate.toggle();
    }

    /// Get all current containers, handle into ContainerItem in the app_data struct rather than here
    /// Just make sure that items sent are guaranteed to have an id
    /// Will ignore any container that uses `./start_oxker.sh` as an entry point, unless the `-s` flag is set
    pub async fn update_all_containers(&mut self) -> Vec<(bool, ContainerId)> {
        let containers = self
            .docker
            .list_containers(Some(ListContainersOptions::<String> {
                all: true,
                ..Default::default()
            }))
            .await
            .unwrap_or_default();

        let mut output = containers
            .into_iter()
            .filter_map(|f| match f.id {
                Some(_) => {
                    if f.command
                        .as_ref()
                        .map_or(false, |c| c.starts_with(ENTRY_POINT))
                        && self.args.show_self
                    {
                        None
                    } else {
                        Some(f)
                    }
                }
                None => None,
            })
            .collect::<Vec<ContainerSummary>>();

        self.app_data.lock().update_containers(&mut output);

        // Just get the containers that are currently running, or being restarted, no point updating info on paused or dead containers
        output
            .into_iter()
            .filter_map(|i| {
                i.id.map(|id| {
                    (
                        i.state == Some("running".to_owned())
                            || i.state == Some("restarting".to_owned()),
                        ContainerId::from(id),
                    )
                })
            })
            .collect::<Vec<_>>()
    }

    /// Update single container logs
    /// don't take &self, so that can tokio::spawn into it's own thread
    /// remove if from spawns hashmap when complete
    async fn update_log(
        docker: Arc<Docker>,
        id: ContainerId,
        timestamps: bool,
        since: u64,
        app_data: Arc<Mutex<AppData>>,
        spawns: Arc<Mutex<HashMap<SpawnId, JoinHandle<()>>>>,
    ) {
        let options = Some(LogsOptions::<String> {
            stdout: true,
            timestamps,
            since: i64::try_from(since).unwrap_or_default(),
            ..Default::default()
        });

        let mut logs = docker.logs(id.get(), options);
        let mut output = vec![];

        while let Some(value) = logs.next().await {
            if let Ok(data) = value {
                let log_string = data.to_string();
                if !log_string.trim().is_empty() {
                    output.push(log_string);
                }
            }
        }
        spawns.lock().remove(&SpawnId::Log(id.clone()));
        app_data.lock().update_log_by_id(&output, &id);
    }

    /// Update all logs, spawn each container into own tokio::spawn thread
    fn init_all_logs(&mut self, all_ids: &[(bool, ContainerId)]) {
        for (_, id) in all_ids {
            let docker = Arc::clone(&self.docker);
            let app_data = Arc::clone(&self.app_data);
            let spawns = Arc::clone(&self.spawns);
            let key = SpawnId::Log(id.clone());
            self.spawns.lock().insert(
                key,
                tokio::spawn(Self::update_log(
                    docker,
                    id.clone(),
                    self.args.timestamp,
                    0,
                    app_data,
                    spawns,
                )),
            );
        }
    }

    /// Update all cpu_mem, and selected container log (if a log update join_handle isn't currently being executed)
    async fn update_everything(&mut self) {
        let all_ids = self.update_all_containers().await;
        let optional_index = self.app_data.lock().get_selected_log_index();
        if let Some(index) = optional_index {
            if let Some(container) = self.app_data.lock().containers.items.get(index) {
                self.spawns
                    .lock()
                    .entry(SpawnId::Log(container.id.clone()))
                    .or_insert_with(|| {
                        let docker = Arc::clone(&self.docker);
                        let app_data = Arc::clone(&self.app_data);
                        let spawns = Arc::clone(&self.spawns);
                        tokio::spawn(Self::update_log(
                            docker,
                            container.id.clone(),
                            self.args.timestamp,
                            container.last_updated,
                            app_data,
                            spawns,
                        ))
                    });
            }
        };
        self.update_all_container_stats(&all_ids);
    }

    /// Animate the loading icon
    async fn loading_spin(&mut self, loading_uuid: Uuid) -> JoinHandle<()> {
        let gui_state = Arc::clone(&self.gui_state);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                gui_state.lock().next_loading(loading_uuid);
            }
        })
    }

    /// Stop the loading_spin function, and reset gui loading status
    fn stop_loading_spin(&mut self, handle: &JoinHandle<()>, loading_uuid: Uuid) {
        handle.abort();
        self.gui_state.lock().remove_loading(loading_uuid);
    }

    /// Initialize docker container data, before any messages are received
    async fn initialise_container_data(&mut self) {
        self.gui_state.lock().status_push(Status::Init);
        let loading_uuid = Uuid::new_v4();
        let loading_spin = self.loading_spin(loading_uuid).await;

        let all_ids = self.update_all_containers().await;

        self.update_all_container_stats(&all_ids);

        // Maybe only do a single one at first?
        self.init_all_logs(&all_ids);

        if all_ids.is_empty() {
            self.initialised = true;
        }

        // wait until all logs have initialised
        while !self.initialised {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            self.initialised = self.app_data.lock().initialised(&all_ids);
        }
        self.gui_state.lock().status_del(Status::Init);
        self.stop_loading_spin(&loading_spin, loading_uuid);
    }

    /// Set the global error as the docker error, and set gui_state to error
    fn set_error(&mut self, error: DockerControls) {
        self.app_data
            .lock()
            .set_error(AppError::DockerCommand(error));
        self.gui_state.lock().status_push(Status::Error);
    }

    /// Execute a docker command, will start and stop the loading spinner, and set correct error
    async fn exec_docker(
        &mut self,
        docker_fn: impl Future<Output = Result<(), bollard::errors::Error>> + Send,
        control: DockerControls,
    ) {
        let uuid = Uuid::new_v4();
        let loading_spin = self.loading_spin(uuid).await;
        if docker_fn.await.is_err() {
            self.set_error(control);
        };
        self.stop_loading_spin(&loading_spin, uuid);
    }

    /// Handle incoming messages, container controls & all container information update
    async fn message_handler(&mut self) {
        while let Some(message) = self.receiver.recv().await {
            let docker = Arc::clone(&self.docker);
            match message {
                DockerMessage::Pause(id) => {
                    self.exec_docker(docker.pause_container(id.get()), DockerControls::Pause)
                        .await;
                }
                DockerMessage::Restart(id) => {
                    self.exec_docker(
                        docker.restart_container(id.get(), None),
                        DockerControls::Restart,
                    )
                    .await;
                }
                DockerMessage::Start(id) => {
                    self.exec_docker(
                        docker.start_container(id.get(), None::<StartContainerOptions<String>>),
                        DockerControls::Start,
                    )
                    .await;
                }
                DockerMessage::Stop(id) => {
                    self.exec_docker(docker.stop_container(id.get(), None), DockerControls::Stop)
                        .await;
                }
                DockerMessage::Unpause(id) => {
                    self.exec_docker(docker.unpause_container(id.get()), DockerControls::Unpause)
                        .await;
                    self.update_everything().await;
                }
                DockerMessage::Update => self.update_everything().await,
                DockerMessage::Quit => {
                    self.spawns
                        .lock()
                        .values()
                        .into_iter()
                        .for_each(tokio::task::JoinHandle::abort);
                    self.is_running.store(false, Ordering::SeqCst);
                }
            }
        }
    }

    /// Initialise self, and start the message receiving loop
    pub async fn init(
        app_data: Arc<Mutex<AppData>>,
        docker: Arc<Docker>,
        gui_state: Arc<Mutex<GuiState>>,
        receiver: Receiver<DockerMessage>,
        is_running: Arc<AtomicBool>,
    ) {
        let args = app_data.lock().args;
        if app_data.lock().get_error().is_none() {
            let mut inner = Self {
                app_data,
                args,
                docker,
                gui_state,
                initialised: false,
                receiver,
                spawns: Arc::new(Mutex::new(HashMap::new())),
                is_running,
                binate: Binate::One,
            };
            inner.initialise_container_data().await;

            inner.message_handler().await;
        }
    }
}

// tests, use redis-test container, check logs exists, and selector of logs, and that it increases, and matches end, when you run restart on the docker containers
