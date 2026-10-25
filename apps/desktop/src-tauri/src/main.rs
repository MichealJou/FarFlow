#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use farflow_core::{Command, GitReview, GitTransport, Id, Mode, Result as DomainResult};
use farflow_desktop::{
    AttachmentInput, ClonedWorkspace, Desktop, Directory, GitRemoteAction, GitRemoteResult,
    OpenedWorkspace, PackFile, PackResult, Profile, ResolutionInput, Saved, Snapshot,
    TransferAction, TransferResult, TransferStage, TransferStaged,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_dialog::DialogExt;

type Shared = Arc<Mutex<Desktop>>;
type Jobs = Arc<Mutex<BTreeMap<Id, GitJob>>>;

#[cfg(target_os = "macos")]
mod macos;

fn finish_jobs<R: Runtime>(app: &AppHandle<R>) {
    let _ = cancel_jobs(app, None);
    let jobs = app.state::<Jobs>();
    while jobs
        .lock()
        .map(|jobs| {
            jobs.values()
                .any(|entry| matches!(entry.status, GitJobStatus::Running))
        })
        .unwrap_or(false)
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[derive(Clone, Serialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum GitJobStatus {
    Running,
    Completed { result: GitRemoteResult },
    Failed { message: String },
    Cloned,
    Packed { result: PackResult },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GitJobScope {
    Workspace(Id),
    Directory(Id),
}

enum GitJobResult {
    Remote(GitRemoteResult),
    Cloned(ClonedWorkspace),
    Packed(PackResult),
}

struct GitJob {
    scope: GitJobScope,
    control: GitTransport,
    status: GitJobStatus,
    closed: bool,
    cloned: Option<ClonedWorkspace>,
}

fn cancel_jobs<R: Runtime>(app: &AppHandle<R>, handle: Option<Id>) -> Result<bool, String> {
    let jobs = app.state::<Jobs>();
    let mut jobs = jobs
        .lock()
        .map_err(|_| "Git 同步状态不可用，请重新打开应用".to_string())?;
    let mut active = false;
    for job in jobs.values_mut() {
        if handle.is_none_or(|handle| job.scope == GitJobScope::Workspace(handle)) {
            job.closed |= handle.is_some();
            job.control.cancel();
            active |= matches!(job.status, GitJobStatus::Running);
        }
    }
    jobs.retain(|_, job| !job.closed || matches!(job.status, GitJobStatus::Running));
    Ok(active)
}

fn spawn_git_job(
    jobs: Jobs,
    scope: GitJobScope,
    job: Id,
    work: impl FnOnce(&GitTransport) -> DomainResult<GitJobResult> + Send + 'static,
) -> DomainResult<()> {
    if job.is_nil() {
        return Err(farflow_core::Error::Validation("同步操作标识无效".into()));
    }
    let control = GitTransport::default();
    {
        let mut jobs = jobs.lock().map_err(|_| {
            farflow_core::Error::Storage("Git 同步状态不可用，请重新打开应用".into())
        })?;
        if jobs.contains_key(&job)
            || jobs
                .values()
                .any(|entry| entry.scope == scope && matches!(entry.status, GitJobStatus::Running))
        {
            return Err(farflow_core::Error::Conflict(
                "当前目录或空间已有同步操作，请等待结束或取消后重试".into(),
            ));
        }
        jobs.retain(|_, entry| entry.scope != scope);
        jobs.insert(
            job,
            GitJob {
                scope,
                control: control.clone(),
                status: GitJobStatus::Running,
                closed: false,
                cloned: None,
            },
        );
    }
    let running = jobs.clone();
    let spawned = std::thread::Builder::new()
        .name("farflow-git".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(&control)));
            let (status, cloned) = match result {
                Ok(Ok(GitJobResult::Remote(result))) => (GitJobStatus::Completed { result }, None),
                Ok(Ok(GitJobResult::Cloned(result))) => (GitJobStatus::Cloned, Some(result)),
                Ok(Ok(GitJobResult::Packed(result))) => (GitJobStatus::Packed { result }, None),
                Ok(Err(error)) => (
                    GitJobStatus::Failed {
                        message: error.to_string(),
                    },
                    None,
                ),
                Err(_) => (
                    GitJobStatus::Failed {
                        message: "Git 操作中断，请核对本机目录与远端结果".into(),
                    },
                    None,
                ),
            };
            if let Ok(mut jobs) = running.lock() {
                if let Some(entry) = jobs.get_mut(&job) {
                    if entry.closed {
                        jobs.remove(&job);
                    } else {
                        entry.status = status;
                        entry.cloned = cloned;
                    }
                }
            }
        });
    if spawned.is_err() {
        if let Ok(mut jobs) = jobs.lock() {
            jobs.remove(&job);
        }
        return Err(farflow_core::Error::Storage(
            "无法开始 Git 操作，请重试".into(),
        ));
    }
    Ok(())
}

fn git_job_status<R: Runtime>(
    app: &AppHandle<R>,
    scope: GitJobScope,
    job: Id,
) -> Result<GitJobStatus, String> {
    let jobs = app.state::<Jobs>();
    let jobs = jobs
        .lock()
        .map_err(|_| "Git 同步状态不可用，请重新打开应用".to_string())?;
    Ok(jobs
        .get(&job)
        .filter(|entry| entry.scope == scope)
        .ok_or("当前目录或空间没有该操作")?
        .status
        .clone())
}

fn cancel_git_job<R: Runtime>(
    app: &AppHandle<R>,
    scope: GitJobScope,
    job: Id,
) -> Result<(), String> {
    let jobs = app.state::<Jobs>();
    let jobs = jobs
        .lock()
        .map_err(|_| "Git 同步状态不可用，请重新打开应用".to_string())?;
    let entry = jobs
        .get(&job)
        .filter(|entry| entry.scope == scope)
        .ok_or("当前目录或空间没有该操作")?;
    entry.control.cancel();
    Ok(())
}

#[tauri::command]
async fn desktop_git_start<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    job: Id,
    action: GitRemoteAction,
    address: String,
) -> Result<(), String> {
    let jobs = app.state::<Jobs>().inner().clone();
    with_host(app, move |host| {
        let session = host.git_session(handle)?;
        spawn_git_job(jobs, GitJobScope::Workspace(handle), job, move |control| {
            session
                .remote(action, &address, control)
                .map(GitJobResult::Remote)
        })
    })
    .await
}

#[tauri::command]
fn desktop_git_status<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    job: Id,
) -> Result<GitJobStatus, String> {
    git_job_status(&app, GitJobScope::Workspace(handle), job)
}

#[tauri::command]
fn desktop_git_cancel<R: Runtime>(app: AppHandle<R>, handle: Id, job: Id) -> Result<(), String> {
    cancel_git_job(&app, GitJobScope::Workspace(handle), job)
}

#[tauri::command]
async fn desktop_clone_start<R: Runtime>(
    app: AppHandle<R>,
    directory: Id,
    folder: String,
    job: Id,
    address: String,
) -> Result<(), String> {
    let jobs = app.state::<Jobs>().inner().clone();
    with_host(app, move |host| {
        let destination = host.git_destination(directory, &folder)?;
        spawn_git_job(
            jobs,
            GitJobScope::Directory(directory),
            job,
            move |control| {
                destination
                    .clone_remote(&address, control)
                    .map(GitJobResult::Cloned)
            },
        )
    })
    .await
}

#[tauri::command]
fn desktop_clone_status<R: Runtime>(
    app: AppHandle<R>,
    directory: Id,
    job: Id,
) -> Result<GitJobStatus, String> {
    git_job_status(&app, GitJobScope::Directory(directory), job)
}

#[tauri::command]
fn desktop_clone_cancel<R: Runtime>(
    app: AppHandle<R>,
    directory: Id,
    job: Id,
) -> Result<(), String> {
    cancel_git_job(&app, GitJobScope::Directory(directory), job)
}

#[tauri::command]
fn desktop_clone_close<R: Runtime>(
    app: AppHandle<R>,
    directory: Id,
    job: Id,
) -> Result<(), String> {
    let jobs = app.state::<Jobs>();
    let mut jobs = jobs
        .lock()
        .map_err(|_| "Git 同步状态不可用，请重新打开应用".to_string())?;
    if let Some(entry) = jobs.get_mut(&job) {
        if entry.scope != GitJobScope::Directory(directory) {
            return Err("当前目录没有该克隆操作".into());
        }
        entry.control.cancel();
        entry.closed = true;
        if !matches!(entry.status, GitJobStatus::Running) {
            jobs.remove(&job);
        }
    }
    Ok(())
}

#[tauri::command]
async fn desktop_clone_open<R: Runtime>(
    app: AppHandle<R>,
    directory: Id,
    job: Id,
) -> Result<OpenedWorkspace, String> {
    let jobs = app.state::<Jobs>().inner().clone();
    with_host(app, move |host| {
        let mut jobs = jobs.lock().map_err(|_| {
            farflow_core::Error::Storage("Git 同步状态不可用，请重新打开应用".into())
        })?;
        let cloned = jobs
            .get(&job)
            .filter(|entry| entry.scope == GitJobScope::Directory(directory) && !entry.closed)
            .and_then(|entry| entry.cloned.as_ref())
            .ok_or_else(|| {
                farflow_core::Error::Conflict("克隆尚未完成或已结束，请核对本机目录".into())
            })?;
        let opened = host.open_cloned(cloned)?;
        jobs.remove(&job);
        Ok(opened)
    })
    .await
}

#[tauri::command]
async fn desktop_clone_pack_start<R: Runtime>(
    app: AppHandle<R>,
    directory: Id,
    folder: String,
    file: Id,
    job: Id,
) -> Result<(), String> {
    let jobs = app.state::<Jobs>().inner().clone();
    with_host(app, move |host| {
        let destination = host.git_destination(directory, &folder)?;
        let source = host.pack_source(file)?;
        spawn_git_job(
            jobs,
            GitJobScope::Directory(directory),
            job,
            move |control| {
                control.check()?;
                destination.clone_pack(source).map(GitJobResult::Cloned)
            },
        )
    })
    .await
}

async fn start_pack_export<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    job: Id,
    path: PathBuf,
) -> Result<(), String> {
    let jobs = app.state::<Jobs>().inner().clone();
    with_host(app, move |host| {
        let session = host.git_session(handle)?;
        spawn_git_job(jobs, GitJobScope::Workspace(handle), job, move |control| {
            control.check()?;
            session.export_pack(&path).map(GitJobResult::Packed)
        })
    })
    .await
}

#[tauri::command]
async fn desktop_pack_export<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    job: Id,
) -> Result<bool, String> {
    with_host(app.clone(), move |host| {
        host.git_session(handle).map(|_| ())
    })
    .await?;
    let dialog = file_dialog(&app)?;
    let picked = tauri::async_runtime::spawn_blocking(move || {
        dialog
            .set_title("导出研流 Git 文件包")
            .add_filter("研流 Git 文件包", &["farflowpack"])
            .set_file_name("FarFlow.farflowpack")
            .blocking_save_file()
    })
    .await
    .map_err(|_| "无法打开文件保存窗口".to_string())?;
    let Some(path) = picked else { return Ok(false) };
    let path = path
        .into_path()
        .map_err(|_| "请选择本机文件位置".to_string())?;
    start_pack_export(app, handle, job, path).await?;
    Ok(true)
}

#[tauri::command]
async fn desktop_pack_import<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    file: Id,
    job: Id,
) -> Result<(), String> {
    let jobs = app.state::<Jobs>().inner().clone();
    with_host(app, move |host| {
        let session = host.git_session(handle)?;
        let source = host.pack_source(file)?;
        spawn_git_job(jobs, GitJobScope::Workspace(handle), job, move |control| {
            control.check()?;
            session.import_pack(source).map(GitJobResult::Packed)
        })
    })
    .await
}

fn file_dialog<R: Runtime>(
    app: &AppHandle<R>,
) -> Result<tauri_plugin_dialog::FileDialogBuilder<R>, String> {
    let parent = app
        .get_webview_window("main")
        .ok_or_else(|| "主窗口不可用，请重新打开应用".to_string())?;
    // 连续选择时不能让对话框库自动把已关闭的选择面板当成所属窗口。
    Ok(app.dialog().file().set_parent(&parent))
}

#[tauri::command]
async fn desktop_select_pack<R: Runtime>(app: AppHandle<R>) -> Result<Option<PackFile>, String> {
    let dialog = file_dialog(&app)?;
    let picked = tauri::async_runtime::spawn_blocking(move || {
        dialog
            .set_title("选择研流 Git 文件包")
            .add_filter("研流 Git 文件包", &["farflowpack"])
            .blocking_pick_file()
    })
    .await
    .map_err(|_| "无法打开文件选择窗口".to_string())?;
    let Some(path) = picked else { return Ok(None) };
    let path = path.into_path().map_err(|_| "请选择本机文件".to_string())?;
    with_host(app, move |host| host.select_pack(&path))
        .await
        .map(Some)
}

#[tauri::command]
async fn desktop_select_transfer_file<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<PackFile>, String> {
    let dialog = file_dialog(&app)?;
    let picked = tauri::async_runtime::spawn_blocking(move || {
        dialog
            .set_title("选择迁移回执或完成凭据")
            .add_filter("迁移 JSON 文件", &["json"])
            .blocking_pick_file()
    })
    .await
    .map_err(|_| "无法打开迁移文件选择窗口".to_string())?;
    let Some(path) = picked else { return Ok(None) };
    let path = path.into_path().map_err(|_| "请选择本机文件".to_string())?;
    with_host(app, move |host| host.select_pack(&path))
        .await
        .map(Some)
}

#[tauri::command]
async fn desktop_transfer_view<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
) -> Result<TransferResult, String> {
    with_host(app, move |host| host.transfer_view(handle)).await
}

#[tauri::command]
async fn desktop_transfer<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    action: TransferAction,
    at: i64,
) -> Result<TransferResult, String> {
    with_host(app, move |host| host.transfer(handle, action, at)).await
}

#[tauri::command]
async fn desktop_inspect_transfer<R: Runtime>(
    app: AppHandle<R>,
    directory: Id,
) -> Result<farflow_core::MigrationReport, String> {
    with_host(app, move |host| host.inspect_transfer(directory)).await
}

#[tauri::command]
async fn desktop_stage_transfer<R: Runtime>(
    app: AppHandle<R>,
    input: TransferStage,
    at: i64,
) -> Result<TransferStaged, String> {
    with_host(app, move |host| host.stage_transfer(input, at)).await
}

async fn with_host<R: Runtime, T: Send + 'static>(
    app: AppHandle<R>,
    work: impl FnOnce(&mut Desktop) -> DomainResult<T> + Send + 'static,
) -> Result<T, String> {
    let shared = app.state::<Shared>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut host = shared
            .lock()
            .map_err(|_| "本机工作空间状态不可用，请重新打开应用".to_string())?;
        work(&mut host).map_err(|error| error.to_string())
    })
    .await
    .map_err(|_| "本机操作中断，请核对工作空间后重试".to_string())?
}

#[tauri::command]
async fn desktop_select_directory<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<Directory>, String> {
    let dialog = file_dialog(&app)?;
    let picked = tauri::async_runtime::spawn_blocking(move || {
        dialog
            .set_title("选择 FarFlow 工作空间目录")
            .blocking_pick_folder()
    })
    .await
    .map_err(|_| "无法打开目录选择窗口".to_string())?;
    let Some(path) = picked else { return Ok(None) };
    let path = path
        .into_path()
        .map_err(|_| "请选择本机文件夹".to_string())?;
    with_host(app, move |host| host.select_directory(&path))
        .await
        .map(Some)
}

#[tauri::command]
async fn desktop_create<R: Runtime>(
    app: AppHandle<R>,
    directory: Id,
    folder: String,
    mode: Mode,
    name: String,
    profile: Profile,
) -> Result<OpenedWorkspace, String> {
    with_host(app, move |host| {
        host.create(directory, &folder, mode, name, profile)
    })
    .await
}

#[tauri::command]
async fn desktop_inspect<R: Runtime>(
    app: AppHandle<R>,
    directory: Id,
    mode: Mode,
) -> Result<OpenedWorkspace, String> {
    with_host(app, move |host| host.inspect(directory, mode)).await
}

#[tauri::command]
async fn desktop_select_identity<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    actor: Id,
) -> Result<Snapshot, String> {
    with_host(app, move |host| host.select_identity(handle, actor)).await
}

#[tauri::command]
async fn desktop_snapshot<R: Runtime>(app: AppHandle<R>, handle: Id) -> Result<Snapshot, String> {
    with_host(app, move |host| host.snapshot(handle)).await
}

#[tauri::command]
async fn desktop_git_review<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
) -> Result<GitReview, String> {
    with_host(app, move |host| host.git_review(handle)).await
}

#[tauri::command]
async fn desktop_git_resolve<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    input: ResolutionInput,
) -> Result<Saved, String> {
    with_host(app, move |host| host.git_resolve(handle, input)).await
}

#[tauri::command]
async fn desktop_git_rebuild<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
) -> Result<Snapshot, String> {
    with_host(app, move |host| host.git_rebuild(handle)).await
}

#[tauri::command(rename_all = "snake_case")]
async fn desktop_execute<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    operation_id: Id,
    command: Command,
    expected_revision: u64,
    at: i64,
) -> Result<Saved, String> {
    with_host(app, move |host| {
        host.execute(handle, operation_id, command, expected_revision, at)
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UploadMessage {
    handle: Id,
    at: i64,
    attachment: AttachmentInput,
}

fn upload_request(request: &tauri::ipc::Request<'_>) -> Result<(UploadMessage, Vec<u8>), String> {
    let header = request
        .headers()
        .get("x-farflow-upload")
        .and_then(|value| value.to_str().ok())
        .ok_or("缺少附件元信息")?;
    let header = percent_encoding::percent_decode_str(header)
        .decode_utf8()
        .map_err(|_| "附件元信息编码无效")?;
    let message = serde_json::from_str(&header).map_err(|_| "附件元信息格式无效")?;
    match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) if bytes.len() <= 10 * 1024 * 1024 => {
            Ok((message, bytes.clone()))
        }
        _ => Err("附件必须为不超过 10 MiB 的原始字节".into()),
    }
}

#[tauri::command]
async fn desktop_upload<R: Runtime>(
    app: AppHandle<R>,
    request: tauri::ipc::Request<'_>,
) -> Result<Saved, String> {
    let (message, bytes) = upload_request(&request)?;
    with_host(app, move |host| {
        host.upload(message.handle, message.attachment, &bytes, message.at)
    })
    .await
}

#[tauri::command]
async fn desktop_attachment<R: Runtime>(
    app: AppHandle<R>,
    handle: Id,
    id: Id,
) -> Result<tauri::ipc::Response, String> {
    with_host(app, move |host| host.attachment(handle, id))
        .await
        .map(tauri::ipc::Response::new)
}

#[tauri::command]
async fn desktop_close<R: Runtime>(app: AppHandle<R>, handle: Id) -> Result<(), String> {
    with_host(app.clone(), move |host| {
        cancel_jobs(&app, Some(handle)).map_err(farflow_core::Error::Storage)?;
        host.close(handle);
        Ok(())
    })
    .await
}

fn builder<R: Runtime>() -> tauri::Builder<R> {
    tauri::Builder::<R>::new()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_http::init())
        .manage(Shared::default())
        .manage(Jobs::default())
        .invoke_handler(tauri::generate_handler![
            desktop_select_directory,
            desktop_select_transfer_file,
            desktop_transfer_view,
            desktop_transfer,
            desktop_inspect_transfer,
            desktop_stage_transfer,
            desktop_create,
            desktop_inspect,
            desktop_select_identity,
            desktop_snapshot,
            desktop_execute,
            desktop_upload,
            desktop_attachment,
            desktop_close,
            desktop_git_start,
            desktop_git_status,
            desktop_git_cancel,
            desktop_git_review,
            desktop_git_resolve,
            desktop_git_rebuild,
            desktop_clone_start,
            desktop_clone_status,
            desktop_clone_cancel,
            desktop_clone_close,
            desktop_clone_open,
            desktop_clone_pack_start,
            desktop_select_pack,
            desktop_pack_export,
            desktop_pack_import
        ])
}

fn main() {
    let app = builder::<tauri::Wry>()
        .build(tauri::generate_context!())
        .expect("FarFlow 桌面启动失败");
    #[cfg(target_os = "macos")]
    let unregister = macos::observe_termination(app.handle().clone());
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            if cancel_jobs(app, None).unwrap_or(false) {
                api.prevent_exit();
                let app = app.clone();
                std::thread::spawn(move || {
                    finish_jobs(&app);
                    app.exit(0);
                });
            }
        }
    });
    #[cfg(target_os = "macos")]
    unregister();
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};
    use tauri::{
        ipc::{CallbackFn, InvokeBody},
        test::{get_ipc_response, mock_context, noop_assets, MockRuntime},
        webview::InvokeRequest,
    };

    #[test]
    fn configured_http_permissions_accept_team_service_ports_and_reject_other_protocols() {
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(tauri::generate_context!())
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        for address in [
            "http://127.0.0.1:4640/api/auth/me",
            "https://farflow.example.com:8443/api/workspaces",
            "http://localhost/api/workspaces",
            "https://farflow.example.com/api/workspaces",
        ] {
            let result = call(
                &view,
                "plugin:http|fetch",
                json!({"clientConfig":{"method":"GET","url":address,"headers":[],"data":null}}),
                app.invoke_key(),
            );
            assert!(result.is_ok(), "{address}: {result:?}");
            call(
                &view,
                "plugin:http|fetch_cancel",
                json!({"rid":result.unwrap()}),
                app.invoke_key(),
            )
            .unwrap();
        }
        assert!(call(&view, "plugin:http|fetch", json!({"clientConfig":{"method":"GET","url":"file:///private/test","headers":[],"data":null}}), app.invoke_key()).is_err());
    }

    fn request(command: &str, body: InvokeBody, key: &str) -> InvokeRequest {
        InvokeRequest {
            cmd: command.into(),
            callback: CallbackFn(0),
            error: CallbackFn(1),
            url: "tauri://localhost".parse().unwrap(),
            body,
            headers: Default::default(),
            invoke_key: key.into(),
        }
    }
    fn call(
        view: &tauri::WebviewWindow<MockRuntime>,
        command: &str,
        body: Value,
        key: &str,
    ) -> Result<Value, Value> {
        get_ipc_response(view, request(command, InvokeBody::Json(body), key))
            .map(|value| value.deserialize().unwrap())
    }

    fn await_pack(
        view: &tauri::WebviewWindow<MockRuntime>,
        key: &str,
        handle: Id,
        job: Id,
    ) -> Value {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status = call(
                view,
                "desktop_git_status",
                json!({"handle":handle,"job":job}),
                key,
            )
            .unwrap();
            if status["phase"] != "running" {
                return status;
            }
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn native_pack_jobs_exchange_full_files_with_scoped_capabilities_and_real_members() {
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        let temp = tempfile::tempdir().unwrap();
        let (directory, handle, actor) = {
            let shared = app.state::<Shared>();
            let mut host = shared.lock().unwrap();
            let directory = host.select_directory(temp.path()).unwrap();
            let opened = host
                .create(
                    directory.token,
                    "source",
                    Mode::Git,
                    "文件包原生验收".into(),
                    Profile {
                        account: "owner".into(),
                        name: "实际所有者".into(),
                        department: "研发".into(),
                    },
                )
                .unwrap();
            let actor = opened.identities[0].id;
            host.select_identity(opened.handle, actor).unwrap();
            (directory.token, opened.handle, actor)
        };
        let item = Id::new_v4();
        call(&view,"desktop_execute",json!({"handle":handle,"operation_id":Id::new_v4(),"command":{"type":"create","item":{"id":item,"kind":"document","title":"文件包文档"}},"expected_revision":0,"at":100}),key).unwrap();
        let first = temp.path().join("first.farflowpack");
        let export = Id::new_v4();
        tauri::async_runtime::block_on(start_pack_export(
            app.handle().clone(),
            handle,
            export,
            first.clone(),
        ))
        .unwrap();
        let result = await_pack(&view, key, handle, export);
        assert_eq!(result["phase"], "packed");
        assert_eq!(result["result"]["action"], "export");
        assert!(result["result"]["bytes"].as_u64().unwrap() > 0);
        let file = app
            .state::<Shared>()
            .lock()
            .unwrap()
            .select_pack(&first)
            .unwrap()
            .token;
        let clone = Id::new_v4();
        assert!(call(
            &view,
            "desktop_clone_pack_start",
            json!({"directory":directory,"folder":"invalid","file":directory,"job":Id::new_v4()}),
            key
        )
        .is_err());
        call(
            &view,
            "desktop_clone_pack_start",
            json!({"directory":directory,"folder":"copy","file":file,"job":clone}),
            key,
        )
        .unwrap();
        assert_eq!(await_clone(&view, key, directory, clone)["phase"], "cloned");
        let opened = call(
            &view,
            "desktop_clone_open",
            json!({"directory":directory,"job":clone}),
            key,
        )
        .unwrap();
        let copy: Id = serde_json::from_value(opened["handle"].clone()).unwrap();
        assert!(call(&view, "desktop_snapshot", json!({"handle":copy}), key).is_err());
        call(
            &view,
            "desktop_select_identity",
            json!({"handle":copy,"actor":actor}),
            key,
        )
        .unwrap();
        let comment = Id::new_v4();
        call(&view,"desktop_execute",json!({"handle":copy,"operation_id":Id::new_v4(),"command":{"type":"add_comment","id":comment,"item_id":item,"body":"离线副本回传","parent_id":null,"mentions":[],"attachments":[]},"expected_revision":1,"at":101}),key).unwrap();
        let returning = temp.path().join("return.farflowpack");
        let export = Id::new_v4();
        tauri::async_runtime::block_on(start_pack_export(
            app.handle().clone(),
            copy,
            export,
            returning.clone(),
        ))
        .unwrap();
        assert_eq!(await_pack(&view, key, copy, export)["phase"], "packed");
        let file = app
            .state::<Shared>()
            .lock()
            .unwrap()
            .select_pack(&returning)
            .unwrap()
            .token;
        for _ in 0..2 {
            let import = Id::new_v4();
            call(
                &view,
                "desktop_pack_import",
                json!({"handle":handle,"file":file,"job":import}),
                key,
            )
            .unwrap();
            let result = await_pack(&view, key, handle, import);
            assert_eq!(result["phase"], "packed");
            assert_eq!(result["result"]["issues"], 0);
            let state = call(&view, "desktop_snapshot", json!({"handle":handle}), key).unwrap();
            assert_eq!(state["state"]["revision"], 2);
            assert_eq!(
                state["state"]["comments"][comment.to_string()]["body"],
                "离线副本回传"
            );
        }
        assert!(call(
            &view,
            "desktop_pack_import",
            json!({"handle":handle,"file":Id::new_v4(),"job":Id::new_v4()}),
            key
        )
        .is_err());
        assert!(call(
            &view,
            "desktop_pack_export",
            json!({"handle":Id::new_v4(),"job":Id::new_v4()}),
            key
        )
        .is_err());
        let repeated = Id::new_v4();
        tauri::async_runtime::block_on(start_pack_export(
            app.handle().clone(),
            copy,
            repeated,
            returning.clone(),
        ))
        .unwrap();
        assert_eq!(await_pack(&view, key, copy, repeated)["phase"], "failed");
        let viewer = Id::new_v4();
        app.state::<Shared>().lock().unwrap().execute(handle,Id::new_v4(),serde_json::from_value(json!({"type":"add_member","member":{"id":viewer,"account":"viewer","name":"只读","department":"研发","roles":["viewer"],"active":true}})).unwrap(),2,102).unwrap();
        call(
            &view,
            "desktop_select_identity",
            json!({"handle":handle,"actor":viewer}),
            key,
        )
        .unwrap();
        let rejected = Id::new_v4();
        let forbidden = temp.path().join("forbidden.farflowpack");
        tauri::async_runtime::block_on(start_pack_export(
            app.handle().clone(),
            handle,
            rejected,
            forbidden.clone(),
        ))
        .unwrap();
        assert_eq!(await_pack(&view, key, handle, rejected)["phase"], "failed");
        assert!(!forbidden.exists());
    }

    #[test]
    fn local_pack_work_already_started_finishes_before_exit_and_closed_jobs_are_cleaned() {
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        let temp = tempfile::tempdir().unwrap();
        let (handle, session) = {
            let shared = app.state::<Shared>();
            let mut host = shared.lock().unwrap();
            let dir = host.select_directory(temp.path()).unwrap();
            let opened = host
                .create(
                    dir.token,
                    "source",
                    Mode::Git,
                    "本机落位".into(),
                    Profile {
                        account: "owner".into(),
                        name: "成员".into(),
                        department: "研发".into(),
                    },
                )
                .unwrap();
            host.select_identity(opened.handle, opened.identities[0].id)
                .unwrap();
            (opened.handle, host.git_session(opened.handle).unwrap())
        };
        let path = temp.path().join("finished.farflowpack");
        let saved = path.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (continue_tx, continue_rx) = std::sync::mpsc::channel();
        let job = Id::new_v4();
        spawn_git_job(
            app.state::<Jobs>().inner().clone(),
            GitJobScope::Workspace(handle),
            job,
            move |control| {
                control.check()?;
                started_tx.send(()).unwrap();
                continue_rx.recv().unwrap();
                session.export_pack(&saved).map(GitJobResult::Packed)
            },
        )
        .unwrap();
        started_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        call(&view, "desktop_close", json!({"handle":handle}), key).unwrap();
        continue_tx.send(()).unwrap();
        finish_jobs(app.handle());
        assert!(path.is_file());
        assert!(app.state::<Jobs>().lock().unwrap().is_empty());
    }

    fn await_clone(
        view: &tauri::WebviewWindow<MockRuntime>,
        key: &str,
        directory: Id,
        job: Id,
    ) -> Value {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status = call(
                view,
                "desktop_clone_status",
                json!({"directory":directory,"job":job}),
                key,
            )
            .unwrap();
            if status["phase"] != "running" {
                return status;
            }
            assert!(std::time::Instant::now() < until);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    #[test]
    fn native_clone_jobs_preserve_identity_and_isolate_directory_scopes() {
        use farflow_core::{GitWorkspace, Request};
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        let temp = tempfile::tempdir().unwrap();
        let (directory, actor) = {
            let shared = app.state::<Shared>();
            let mut host = shared.lock().unwrap();
            let directory = host.select_directory(temp.path()).unwrap();
            let source = host
                .create(
                    directory.token,
                    "source",
                    Mode::Git,
                    "已有团队".into(),
                    Profile {
                        account: "owner".into(),
                        name: "原成员".into(),
                        department: "研发".into(),
                    },
                )
                .unwrap();
            (directory.token, source.identities[0].id)
        };
        let source = GitWorkspace::open(temp.path().join("source")).unwrap();
        let item = Id::new_v4();
        source.execute(Request{id:Id::new_v4(),actor_id:actor,at:100,command:serde_json::from_value(json!({"type":"create","item":{"id":item,"kind":"document","title":"原生克隆验收"}})).unwrap()}).unwrap();
        let remote = temp.path().join("remote.git");
        assert!(std::process::Command::new("git")
            .args(["init", "--bare", "--quiet", "--initial-branch=main"])
            .arg(&remote)
            .status()
            .unwrap()
            .success());
        source.push(actor, remote.to_str().unwrap()).unwrap();
        let job = Id::new_v4();
        let start = json!({"directory":directory,"folder":"clone","job":job,"address":remote});
        let mut nil = start.clone();
        nil["job"] = json!(Id::nil());
        assert!(call(&view, "desktop_clone_start", nil, key).is_err());
        let mut unauthorized = start.clone();
        unauthorized["directory"] = json!(Id::new_v4());
        assert!(call(&view, "desktop_clone_start", unauthorized, key).is_err());
        assert!(call(
            &view,
            "desktop_clone_open",
            json!({"directory":directory,"job":job}),
            key
        )
        .is_err());
        call(&view, "desktop_clone_start", start.clone(), key).unwrap();
        assert!(call(&view, "desktop_clone_start", start, key).is_err());
        for command in [
            "desktop_clone_status",
            "desktop_clone_cancel",
            "desktop_clone_close",
            "desktop_clone_open",
        ] {
            assert!(call(
                &view,
                command,
                json!({"directory":Id::new_v4(),"job":job}),
                key
            )
            .is_err());
        }
        for command in ["desktop_git_status", "desktop_git_cancel"] {
            assert!(call(&view, command, json!({"handle":directory,"job":job}), key).is_err());
        }
        assert_eq!(await_clone(&view, key, directory, job)["phase"], "cloned");
        call(
            &view,
            "desktop_clone_cancel",
            json!({"directory":directory,"job":job}),
            key,
        )
        .unwrap();
        let opened = call(
            &view,
            "desktop_clone_open",
            json!({"directory":directory,"job":job}),
            key,
        )
        .unwrap();
        let handle = opened["handle"].clone();
        assert_eq!(opened["identities"][0]["id"], json!(actor));
        assert!(call(&view, "desktop_snapshot", json!({"handle":handle}), key).is_err());
        let data = call(
            &view,
            "desktop_select_identity",
            json!({"handle":handle,"actor":actor}),
            key,
        )
        .unwrap();
        assert_eq!(
            data["state"]["items"][item.to_string()]["title"],
            "原生克隆验收"
        );
        assert!(call(
            &view,
            "desktop_clone_open",
            json!({"directory":directory,"job":job}),
            key
        )
        .is_err());
        let abandoned = Id::new_v4();
        call(
            &view,
            "desktop_clone_start",
            json!({"directory":directory,"folder":"retained","job":abandoned,"address":remote}),
            key,
        )
        .unwrap();
        assert_eq!(
            await_clone(&view, key, directory, abandoned)["phase"],
            "cloned"
        );
        call(
            &view,
            "desktop_clone_close",
            json!({"directory":directory,"job":abandoned}),
            key,
        )
        .unwrap();
        assert!(temp.path().join("retained/workspace.json").is_file());
        assert!(call(
            &view,
            "desktop_clone_status",
            json!({"directory":directory,"job":abandoned}),
            key
        )
        .is_err());
        call(
            &view,
            "desktop_clone_close",
            json!({"directory":directory,"job":abandoned}),
            key,
        )
        .unwrap();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let state = app.state::<Jobs>();
            let _guard = state.lock().unwrap();
            panic!("模拟克隆任务状态中断");
        }));
        for (command, body) in [
            (
                "desktop_clone_start",
                json!({"directory":directory,"folder":"poisoned","job":Id::new_v4(),"address":remote}),
            ),
            (
                "desktop_clone_status",
                json!({"directory":directory,"job":job}),
            ),
            (
                "desktop_clone_cancel",
                json!({"directory":directory,"job":job}),
            ),
            (
                "desktop_clone_close",
                json!({"directory":directory,"job":job}),
            ),
            (
                "desktop_clone_open",
                json!({"directory":directory,"job":job}),
            ),
        ] {
            assert!(call(&view, command, body, key)
                .unwrap_err()
                .as_str()
                .unwrap()
                .contains("同步状态不可用"));
        }
    }

    #[test]
    fn native_clone_cancellation_close_and_exit_release_real_network_connections() {
        use std::{
            io::Read,
            net::TcpListener,
            time::{Duration, Instant},
        };
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        let temp = tempfile::tempdir().unwrap();
        let directory = app
            .state::<Shared>()
            .lock()
            .unwrap()
            .select_directory(temp.path())
            .unwrap()
            .token;
        for ending in ["cancel", "close", "exit"] {
            let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = format!("http://{}/repo.git", listener.local_addr().unwrap());
            let job = Id::new_v4();
            call(
                &view,
                "desktop_clone_start",
                json!({"directory":directory,"folder":ending,"job":job,"address":address}),
                key,
            )
            .unwrap();
            let until = Instant::now() + Duration::from_secs(5);
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < until);
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("连接失败：{error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(stream.read(&mut request).unwrap() > 0);
            assert_eq!(
                call(
                    &view,
                    "desktop_clone_status",
                    json!({"directory":directory,"job":job}),
                    key
                )
                .unwrap()["phase"],
                "running"
            );
            assert!(call(
                &view,
                "desktop_clone_open",
                json!({"directory":directory,"job":job}),
                key
            )
            .is_err());
            match ending {
                "cancel" => {
                    call(
                        &view,
                        "desktop_clone_cancel",
                        json!({"directory":directory,"job":job}),
                        key,
                    )
                    .unwrap();
                    assert_eq!(await_clone(&view, key, directory, job)["phase"], "failed");
                }
                "close" => {
                    call(
                        &view,
                        "desktop_clone_close",
                        json!({"directory":directory,"job":job}),
                        key,
                    )
                    .unwrap();
                    finish_jobs(app.handle());
                    assert!(call(
                        &view,
                        "desktop_clone_status",
                        json!({"directory":directory,"job":job}),
                        key
                    )
                    .is_err());
                }
                _ => {
                    finish_jobs(app.handle());
                    assert_eq!(await_clone(&view, key, directory, job)["phase"], "failed");
                }
            }
            let mut remaining = Vec::new();
            stream.read_to_end(&mut remaining).unwrap();
            assert!(!temp.path().join(ending).exists());
        }
    }

    #[test]
    fn native_git_review_and_decision_use_the_actual_ipc_session() {
        use farflow_core::{GitWorkspace, Request};
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        let temp = tempfile::tempdir().unwrap();
        let (handle, actor) = {
            let shared = app.state::<Shared>();
            let mut host = shared.lock().unwrap();
            let directory = host.select_directory(temp.path()).unwrap();
            let opened = host
                .create(
                    directory.token,
                    "git",
                    Mode::Git,
                    "原生冲突".into(),
                    Profile {
                        account: "owner".into(),
                        name: "实际成员".into(),
                        department: "研发".into(),
                    },
                )
                .unwrap();
            let actor = opened.identities[0].id;
            host.select_identity(opened.handle, actor).unwrap();
            (opened.handle, actor)
        };
        let workspace = GitWorkspace::open(temp.path().join("git")).unwrap();
        let item = Id::new_v4();
        workspace.execute(Request{id:Id::new_v4(),actor_id:actor,at:100,command:serde_json::from_value(json!({"type":"create","item":{"id":item,"kind":"document","title":"原始文档"}})).unwrap()}).unwrap();
        let pack = temp.path().join("copy.pack");
        workspace.export_pack(actor, &pack).unwrap();
        let peer = GitWorkspace::from_pack(&pack, temp.path().join("peer")).unwrap();
        for (workspace, text) in [(&workspace, "甲正文"), (&peer, "乙正文")] {
            workspace
                .execute(Request {
                    id: Id::new_v4(),
                    actor_id: actor,
                    at: 101,
                    command: serde_json::from_value(
                        json!({"type":"update","id":item,"version":1,"patch":{"description":text}}),
                    )
                    .unwrap(),
                })
                .unwrap();
        }
        workspace
            .merge_from(actor, temp.path().join("peer"))
            .unwrap();
        let review = call(&view, "desktop_git_review", json!({"handle":handle}), key).unwrap();
        assert_eq!(review["issues"].as_object().unwrap().len(), 2);
        let rebuilt = call(&view, "desktop_git_rebuild", json!({"handle":handle}), key).unwrap();
        assert_eq!(rebuilt["issues"].as_array().unwrap().len(), 2);
        let targets = review["issues"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>();
        let decision = json!({"id":Id::new_v4(),"token":review["token"],"targets":targets,"command":null,"note":"比较后保留已生效内容","at":200});
        assert!(call(
            &view,
            "desktop_git_resolve",
            json!({"handle":Id::new_v4(),"input":decision}),
            key
        )
        .is_err());
        let saved = call(
            &view,
            "desktop_git_resolve",
            json!({"handle":handle,"input":decision}),
            key,
        )
        .unwrap();
        assert_eq!(saved["revision"], 1);
        let history = call(&view, "desktop_git_review", json!({"handle":handle}), key).unwrap();
        assert_eq!(history["issues"], json!({}));
        assert_eq!(history["decisions"].as_array().unwrap().len(), 1);
        assert_eq!(history["decisions"][0]["applied"], true);
        assert_eq!(
            history["decisions"][0]["targets"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            history["decisions"][0]["operation"]["change"]["resolution"]["actor_id"],
            json!(actor)
        );
        call(
            &view,
            "desktop_git_resolve",
            json!({"handle":handle,"input":decision}),
            key,
        )
        .unwrap();
        call(&view, "desktop_close", json!({"handle":handle}), key).unwrap();
        assert!(call(&view, "desktop_git_rebuild", json!({"handle":handle}), key).is_err());
    }

    #[test]
    fn native_git_jobs_run_without_holding_the_business_session_and_keep_scoped_status() {
        use std::{
            process::Command as Process,
            time::{Duration, Instant},
        };
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        let temp = tempfile::tempdir().unwrap();
        let (handle, actor) = {
            let shared = app.state::<Shared>();
            let mut host = shared.lock().unwrap();
            let directory = host.select_directory(temp.path()).unwrap();
            let opened = host
                .create(
                    directory.token,
                    "git",
                    Mode::Git,
                    "任务验收".into(),
                    Profile {
                        account: "owner".into(),
                        name: "同步成员".into(),
                        department: "研发".into(),
                    },
                )
                .unwrap();
            let actor = opened.identities[0].id;
            host.select_identity(opened.handle, actor).unwrap();
            (opened.handle, actor)
        };
        let remote = temp.path().join("remote.git");
        std::fs::create_dir(&remote).unwrap();
        assert!(Process::new("git")
            .arg("-C")
            .arg(&remote)
            .args(["init", "--bare", "--quiet", "--initial-branch=main"])
            .status()
            .unwrap()
            .success());
        let job = Id::new_v4();
        assert!(call(
            &view,
            "desktop_git_start",
            json!({"handle":handle,"job":Id::nil(),"action":"sync","address":remote}),
            key
        )
        .is_err());
        call(
            &view,
            "desktop_git_start",
            json!({"handle":handle,"job":job,"action":"sync","address":remote}),
            key,
        )
        .unwrap();
        assert_eq!(
            call(
                &view,
                "desktop_git_status",
                json!({"handle":handle,"job":job}),
                key
            )
            .unwrap()["phase"],
            "running"
        );
        assert!(call(
            &view,
            "desktop_git_start",
            json!({"handle":handle,"job":Id::new_v4(),"action":"sync","address":remote}),
            key
        )
        .is_err());
        assert!(call(
            &view,
            "desktop_git_status",
            json!({"handle":Id::new_v4(),"job":job}),
            key
        )
        .is_err());
        assert!(call(
            &view,
            "desktop_git_cancel",
            json!({"handle":Id::new_v4(),"job":job}),
            key
        )
        .is_err());
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let result = call(
                &view,
                "desktop_git_status",
                json!({"handle":handle,"job":job}),
                key,
            )
            .unwrap();
            if result["phase"] != "running" {
                assert_eq!(result["phase"], "completed");
                assert_eq!(result["result"]["push"]["pushed"], true);
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!cancel_jobs(app.handle(), None).unwrap());
        call(
            &view,
            "desktop_git_cancel",
            json!({"handle":handle,"job":job}),
            key,
        )
        .unwrap();
        let failed = Id::new_v4();
        call(
            &view,
            "desktop_git_start",
            json!({"handle":handle,"job":failed,"action":"pull","address":""}),
            key,
        )
        .unwrap();
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let result = call(
                &view,
                "desktop_git_status",
                json!({"handle":handle,"job":failed}),
                key,
            )
            .unwrap();
            if result["phase"] != "running" {
                assert_eq!(result["phase"], "failed");
                assert!(result["message"].as_str().unwrap().contains("地址"));
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(call(
            &view,
            "desktop_git_status",
            json!({"handle":handle,"job":job}),
            key
        )
        .is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let item = Id::new_v4();
            app.state::<Shared>().lock().unwrap().execute(handle,Id::new_v4(),serde_json::from_value(json!({"type":"create","item":{"id":item,"kind":"document","title":"后台同步期间可读"}})).unwrap(),0,1792900000).unwrap();
            let marker = temp.path().join("upload-started");
            let hook = remote.join("hooks/pre-receive");
            std::fs::write(
                &hook,
                format!(
                    "#!/bin/sh\nprintf started > '{}'\nsleep 30\n",
                    marker.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
            for close in [false, true] {
                let job = Id::new_v4();
                call(
                    &view,
                    "desktop_git_start",
                    json!({"handle":handle,"job":job,"action":"push","address":remote}),
                    key,
                )
                .unwrap();
                let until = Instant::now() + Duration::from_secs(3);
                while !marker.exists() && Instant::now() < until {
                    std::thread::sleep(Duration::from_millis(10));
                }
                assert!(marker.exists());
                let read_at = Instant::now();
                assert_eq!(
                    call(&view, "desktop_snapshot", json!({"handle":handle}), key).unwrap()
                        ["actor_id"],
                    actor.to_string()
                );
                assert!(read_at.elapsed() < Duration::from_secs(1));
                if close {
                    call(&view, "desktop_close", json!({"handle":handle}), key).unwrap();
                } else {
                    #[cfg(target_os = "macos")]
                    {
                        let unregister = macos::observe_termination(app.handle().clone());
                        unsafe {
                            objc2_foundation::NSNotificationCenter::defaultCenter()
                                .postNotificationName_object(
                                    objc2_app_kit::NSApplicationWillTerminateNotification,
                                    None,
                                );
                        }
                        unregister();
                    }
                    #[cfg(not(target_os = "macos"))]
                    assert!(cancel_jobs(app.handle(), None).unwrap());
                }
                let until = Instant::now() + Duration::from_secs(3);
                loop {
                    let result = call(
                        &view,
                        "desktop_git_status",
                        json!({"handle":handle,"job":job}),
                        key,
                    );
                    if close && result.is_err() {
                        break;
                    }
                    let result = result.unwrap();
                    if result["phase"] != "running" {
                        assert_eq!(result["phase"], "failed");
                        assert!(result["message"].as_str().unwrap().contains("已取消"));
                        break;
                    }
                    assert!(Instant::now() < until);
                    std::thread::sleep(Duration::from_millis(10));
                }
                if !close {
                    std::fs::remove_file(&marker).unwrap();
                }
            }
        }
    }

    #[test]
    fn poisoned_git_registry_refuses_start_status_cancel_and_close() {
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let temp = tempfile::tempdir().unwrap();
        let handle = {
            let shared = app.state::<Shared>();
            let mut host = shared.lock().unwrap();
            let directory = host.select_directory(temp.path()).unwrap();
            let opened = host
                .create(
                    directory.token,
                    "git",
                    Mode::Git,
                    "异常状态验收".into(),
                    Profile {
                        account: "owner".into(),
                        name: "成员".into(),
                        department: "".into(),
                    },
                )
                .unwrap();
            host.select_identity(opened.handle, opened.identities[0].id)
                .unwrap();
            opened.handle
        };
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let jobs = app.state::<Jobs>();
            let _guard = jobs.lock().unwrap();
            panic!("模拟同步状态锁中断");
        }));
        let job = Id::new_v4();
        for (command, body) in [
            (
                "desktop_git_start",
                json!({"handle":handle,"job":job,"action":"sync","address":"https://farflow.example/repo.git"}),
            ),
            ("desktop_git_status", json!({"handle":handle,"job":job})),
            ("desktop_git_cancel", json!({"handle":handle,"job":job})),
            ("desktop_close", json!({"handle":handle})),
        ] {
            assert!(call(&view, command, body, app.invoke_key())
                .unwrap_err()
                .as_str()
                .unwrap()
                .contains("同步状态不可用"));
        }
    }

    #[test]
    fn ipc_commands_use_the_same_disk_session_and_raw_attachment_bytes() {
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        let temp = tempfile::tempdir().unwrap();
        let shared = app.state::<Shared>();
        let directory = shared
            .lock()
            .unwrap()
            .select_directory(temp.path())
            .unwrap();
        for mode in ["local", "git"] {
            let opened=call(&view,"desktop_create",json!({"directory":directory.token,"folder":mode,"mode":mode,"name":"原生验收","profile":{"account":"owner","name":"原生成员","department":"研发"}}),key).unwrap();
            let handle = opened["handle"].clone();
            let actor = opened["identities"][0]["id"].clone();
            assert!(
                call(&view, "desktop_snapshot", json!({"handle":handle}), key)
                    .unwrap_err()
                    .as_str()
                    .unwrap()
                    .contains("选择")
            );
            let selected = call(
                &view,
                "desktop_select_identity",
                json!({"handle":handle,"actor":actor}),
                key,
            )
            .unwrap();
            assert_eq!(selected["state"]["revision"], 0);
            let item = Id::new_v4();
            let operation = Id::new_v4();
            let created=call(&view,"desktop_execute",json!({"handle":handle,"operation_id":operation,"command":{"type":"create","item":{"id":item,"kind":"document","title":"原生文档"}},"expected_revision":0,"at":1792900000}),key).unwrap();
            assert_eq!(created["revision"], 1);
            let attachment = Id::new_v4();
            let metadata = json!({"handle":handle,"at":1792900000,"attachment":{"operation_id":Id::new_v4(),"id":attachment,"item_id":item,"name":"原生附件.txt","mime":"text/plain"}});
            let encoded = percent_encoding::utf8_percent_encode(
                &metadata.to_string(),
                percent_encoding::NON_ALPHANUMERIC,
            )
            .to_string();
            let bytes = "完整原始字节".as_bytes().to_vec();
            let mut upload = request("desktop_upload", InvokeBody::Raw(bytes.clone()), key);
            upload
                .headers
                .insert("x-farflow-upload", encoded.parse().unwrap());
            assert_eq!(
                get_ipc_response(&view, upload)
                    .unwrap()
                    .deserialize::<Value>()
                    .unwrap()["revision"],
                2
            );
            let result = get_ipc_response(
                &view,
                request(
                    "desktop_attachment",
                    InvokeBody::Json(json!({"handle":handle,"id":attachment})),
                    key,
                ),
            )
            .unwrap();
            assert_eq!(result.deserialize::<Vec<u8>>().unwrap(), bytes);
            let state = call(&view, "desktop_snapshot", json!({"handle":handle}), key).unwrap();
            assert_eq!(
                state["state"]["items"][item.to_string()]["title"],
                "原生文档"
            );
            let selected_directory = shared
                .lock()
                .unwrap()
                .select_directory(&temp.path().join(mode))
                .unwrap();
            let checked = call(
                &view,
                "desktop_inspect",
                json!({"directory":selected_directory.token,"mode":mode}),
                key,
            )
            .unwrap();
            assert_eq!(checked["workspace"]["id"], opened["workspace"]["id"]);
            call(&view, "desktop_close", json!({"handle":handle}), key).unwrap();
            assert!(call(&view, "desktop_snapshot", json!({"handle":handle}), key).is_err());
        }
    }

    #[test]
    fn raw_attachment_boundary_rejects_missing_malformed_and_oversized_requests() {
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        assert!(get_ipc_response(
            &view,
            request("desktop_upload", InvokeBody::Raw(vec![]), key)
        )
        .unwrap_err()
        .as_str()
        .unwrap()
        .contains("缺少"));
        for (header, expected) in [("%ff", "编码"), ("not-json", "格式")] {
            let mut request = request("desktop_upload", InvokeBody::Raw(vec![]), key);
            request
                .headers
                .insert("x-farflow-upload", header.parse().unwrap());
            assert!(get_ipc_response(&view, request)
                .unwrap_err()
                .as_str()
                .unwrap()
                .contains(expected));
        }
        let message=json!({"handle":Id::new_v4(),"at":1,"attachment":{"operation_id":Id::new_v4(),"id":Id::new_v4(),"item_id":Id::new_v4(),"name":"测试.txt","mime":"text/plain"}}).to_string();
        let header =
            percent_encoding::utf8_percent_encode(&message, percent_encoding::NON_ALPHANUMERIC)
                .to_string();
        for body in [
            InvokeBody::Json(json!({})),
            InvokeBody::Raw(vec![0; 10 * 1024 * 1024 + 1]),
        ] {
            let mut request = request("desktop_upload", body, key);
            request
                .headers
                .insert("x-farflow-upload", header.parse().unwrap());
            assert!(get_ipc_response(&view, request)
                .unwrap_err()
                .as_str()
                .unwrap()
                .contains("10 MiB"));
        }
    }

    #[test]
    fn interrupted_native_work_returns_an_error_and_poisoned_sessions_refuse_further_writes() {
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        let interrupted = tauri::async_runtime::block_on(with_host::<MockRuntime, ()>(
            app.handle().clone(),
            |_| panic!("模拟原生操作中断"),
        ));
        assert!(interrupted.unwrap_err().contains("本机操作中断"));
        let id = Id::new_v4();
        for (command, body) in [
            (
                "desktop_create",
                json!({"directory":id,"folder":"data","mode":"local","name":"研发","profile":{"account":"owner","name":"成员","department":""}}),
            ),
            ("desktop_inspect", json!({"directory":id,"mode":"local"})),
            ("desktop_select_identity", json!({"handle":id,"actor":id})),
            ("desktop_snapshot", json!({"handle":id})),
            ("desktop_git_review", json!({"handle":id})),
            ("desktop_git_rebuild", json!({"handle":id})),
            (
                "desktop_git_resolve",
                json!({"handle":id,"input":{"id":id,"token":"records","targets":[id],"command":null,"note":"比较后保留","at":1}}),
            ),
            (
                "desktop_execute",
                json!({"handle":id,"operation_id":id,"command":{"type":"restore","id":id,"version":1},"expected_revision":0,"at":1}),
            ),
            ("desktop_attachment", json!({"handle":id,"id":id})),
            ("desktop_close", json!({"handle":id})),
        ] {
            assert!(call(&view, command, body, key)
                .unwrap_err()
                .as_str()
                .unwrap()
                .contains("状态不可用"));
        }
        let metadata=json!({"handle":id,"at":1,"attachment":{"operation_id":id,"id":id,"item_id":id,"name":"附件","mime":"text/plain"}}).to_string();
        let mut upload = request("desktop_upload", InvokeBody::Raw(vec![1]), key);
        upload.headers.insert(
            "x-farflow-upload",
            percent_encoding::utf8_percent_encode(&metadata, percent_encoding::NON_ALPHANUMERIC)
                .to_string()
                .parse()
                .unwrap(),
        );
        assert!(get_ipc_response(&view, upload)
            .unwrap_err()
            .as_str()
            .unwrap()
            .contains("状态不可用"));
    }

    #[test]
    fn malformed_ipc_arguments_are_rejected_before_any_workspace_operation() {
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let key = app.invoke_key();
        let id = Id::new_v4();
        let calls = [
            (
                "desktop_create",
                json!({"directory":id,"folder":"data","mode":"local","name":"研发","profile":{"account":"owner","name":"成员","department":""}}),
            ),
            ("desktop_inspect", json!({"directory":id,"mode":"local"})),
            ("desktop_select_identity", json!({"handle":id,"actor":id})),
            ("desktop_snapshot", json!({"handle":id})),
            (
                "desktop_execute",
                json!({"handle":id,"operation_id":id,"command":{"type":"restore","id":id,"version":1},"expected_revision":0,"at":1}),
            ),
            ("desktop_attachment", json!({"handle":id,"id":id})),
            ("desktop_close", json!({"handle":id})),
            (
                "desktop_git_start",
                json!({"handle":id,"job":id,"action":"sync","address":"https://farflow.example/repo.git"}),
            ),
            ("desktop_git_status", json!({"handle":id,"job":id})),
            ("desktop_git_cancel", json!({"handle":id,"job":id})),
            (
                "desktop_clone_start",
                json!({"directory":id,"folder":"clone","job":id,"address":"/远端.git"}),
            ),
            ("desktop_clone_status", json!({"directory":id,"job":id})),
            ("desktop_clone_cancel", json!({"directory":id,"job":id})),
            ("desktop_clone_close", json!({"directory":id,"job":id})),
            ("desktop_clone_open", json!({"directory":id,"job":id})),
            (
                "desktop_clone_pack_start",
                json!({"directory":id,"folder":"copy","file":id,"job":id}),
            ),
            (
                "desktop_pack_import",
                json!({"handle":id,"file":id,"job":id}),
            ),
            ("desktop_pack_export", json!({"handle":id,"job":id})),
            ("desktop_git_review", json!({"handle":id})),
            ("desktop_git_rebuild", json!({"handle":id})),
            (
                "desktop_git_resolve",
                json!({"handle":id,"input":{"id":id,"token":"records","targets":[id],"command":null,"note":"比较后保留","at":1}}),
            ),
        ];
        for (command, valid) in calls {
            for field in valid.as_object().unwrap().keys() {
                let mut missing = valid.clone();
                missing.as_object_mut().unwrap().remove(field);
                assert!(
                    call(&view, command, missing, key).is_err(),
                    "{command}: {field}"
                );
                let mut invalid = valid.clone();
                invalid[field] = Value::Null;
                assert!(
                    call(&view, command, invalid, key).is_err(),
                    "{command}: {field}"
                );
            }
        }
    }
    #[test]
    fn migration_ipc_keeps_grants_actual_actor_and_readonly_stage_until_activation() {
        let temp = tempfile::tempdir().unwrap();
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        let view = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        let parent = app
            .state::<Shared>()
            .lock()
            .unwrap()
            .select_directory(temp.path())
            .unwrap()
            .token;
        let opened = call(&view,"desktop_create",json!({"directory":parent,"folder":"source","mode":"local","name":"原生迁移分发","profile":{"account":"owner","name":"实际所有者","department":"研发"}}),app.invoke_key()).unwrap();
        let handle = opened["handle"].as_str().unwrap();
        let actor = opened["identities"][0]["id"].as_str().unwrap();
        call(
            &view,
            "desktop_select_identity",
            json!({"handle":handle,"actor":actor}),
            app.invoke_key(),
        )
        .unwrap();
        let initial = call(
            &view,
            "desktop_transfer_view",
            json!({"handle":handle}),
            app.invoke_key(),
        )
        .unwrap();
        assert!(initial["status"].is_null());
        let id = Id::new_v4();
        call(&view,"desktop_transfer",json!({"handle":handle,"at":10,"action":{"type":"prepare","id":id,"target":"git","directory":parent,"folder":"backup"}}),app.invoke_key()).unwrap();
        call(&view,"desktop_transfer",json!({"handle":handle,"at":11,"action":{"type":"export","directory":parent,"folder":"archive"}}),app.invoke_key()).unwrap();
        let source = app
            .state::<Shared>()
            .lock()
            .unwrap()
            .select_directory(&temp.path().join("archive"))
            .unwrap()
            .token;
        let report = call(
            &view,
            "desktop_inspect_transfer",
            json!({"directory":source}),
            app.invoke_key(),
        )
        .unwrap();
        assert_eq!(report["source"]["id"], opened["workspace"]["id"]);
        let staged = call(&view,"desktop_stage_transfer",json!({"at":12,"input":{"source":source,"directory":parent,"folder":"target","actor":actor,"receipt_directory":parent,"receipt_file":"receipt.json"}}),app.invoke_key()).unwrap();
        let target = staged["opened"]["handle"].as_str().unwrap();
        let snapshot = call(
            &view,
            "desktop_select_identity",
            json!({"handle":target,"actor":actor}),
            app.invoke_key(),
        )
        .unwrap();
        assert_eq!(snapshot["transfer"]["phase"], "staged");
        let receipt = app
            .state::<Shared>()
            .lock()
            .unwrap()
            .select_pack(&temp.path().join("receipt.json"))
            .unwrap()
            .token;
        let complete = call(&view,"desktop_transfer",json!({"handle":handle,"at":13,"action":{"type":"complete","receipt":receipt,"directory":parent,"file":"activation.json"}}),app.invoke_key()).unwrap();
        assert_eq!(complete["status"]["phase"], "completed");
        assert!(!complete.to_string().contains("\"token\""));
        let activation = app
            .state::<Shared>()
            .lock()
            .unwrap()
            .select_pack(&temp.path().join("activation.json"))
            .unwrap()
            .token;
        let result = call(
            &view,
            "desktop_transfer",
            json!({"handle":target,"at":14,"action":{"type":"activate","activation":activation}}),
            app.invoke_key(),
        )
        .unwrap();
        assert_eq!(result["status"]["phase"], "active");
        assert!(call(
            &view,
            "desktop_inspect_transfer",
            json!({"directory":Id::new_v4()}),
            app.invoke_key()
        )
        .is_err());
        assert!(call(
            &view,
            "desktop_transfer",
            json!({"handle":handle,"at":15,"action":{"type":"cancel","id":id,"path":"/任意路径"}}),
            app.invoke_key()
        )
        .is_err());
    }

    #[test]
    fn file_dialog_is_bound_to_the_actual_main_window_and_refuses_a_missing_parent() {
        let app = builder::<MockRuntime>()
            .enable_macos_default_menu(false)
            .build(mock_context(noop_assets()))
            .unwrap();
        assert_eq!(
            file_dialog(app.handle()).err().unwrap(),
            "主窗口不可用，请重新打开应用"
        );
        let _window = tauri::WebviewWindowBuilder::new(&app, "main", Default::default())
            .build()
            .unwrap();
        assert!(file_dialog(app.handle()).is_ok());
    }
}
