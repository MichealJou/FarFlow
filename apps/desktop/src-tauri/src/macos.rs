use block2::RcBlock;
use objc2_app_kit::NSApplicationWillTerminateNotification;
use objc2_foundation::{NSNotification, NSNotificationCenter};
use std::ptr::NonNull;
use tauri::{AppHandle, Runtime};

pub(super) fn observe_termination<R: Runtime>(app: AppHandle<R>) -> impl FnOnce() {
    let center = NSNotificationCenter::defaultCenter();
    let block = RcBlock::new(move |_: NonNull<NSNotification>| super::finish_jobs(&app));
    // 系统退出不会总是经过 Tauri 的退出请求；通知回调只等待不依赖界面线程的后台任务。
    let observer = unsafe {
        center.addObserverForName_object_queue_usingBlock(
            Some(NSApplicationWillTerminateNotification),
            None,
            None,
            &block,
        )
    };
    move || unsafe { center.removeObserver((*observer).as_ref()) }
}
