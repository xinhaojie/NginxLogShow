use flexi_logger::{Age, Cleanup, Criterion, FileSpec, Logger, Naming, DeferredNow, Record, Duplicate};
use nu_ansi_term::Color;
use std::io;

// 1. 控制台格式化函数：带颜色 + 超过100字符截断
fn format_with_color(w: &mut dyn io::Write, now: &mut DeferredNow, rec: &Record) -> io::Result<()> {
    let level_color = match rec.level() {
        log::Level::Error => Color::Red,
        log::Level::Warn => Color::Yellow,
        log::Level::Info => Color::Blue,
        log::Level::Debug => Color::Green,
        log::Level::Trace => Color::Purple,
    };

    // 核心逻辑：将日志内容转为字符串，并处理截断
    let msg = format!("{}", rec.args());
    // 替换 format_with_color 中的截断逻辑为：
    let truncated_msg = if msg.chars().count() > 100 {
        let safe_msg: String = msg.chars().take(100).collect();
        format!("{}...", safe_msg)
    } else {
        msg
    };

    write!(
        w,
        "[{} {:<5} {}:{} {}] {}",
        now.format("%Y-%m-%d %H:%M:%S%.3f"),
        level_color.paint(format!("{:<5}", rec.level())),
        rec.file().unwrap_or("unknown"),
        rec.line().unwrap_or(0),
        rec.target(),
        truncated_msg // 使用处理后的消息
    )
}

// 2. 文件格式化函数：纯文本 + 完整输出
fn format_for_file(w: &mut dyn io::Write, now: &mut DeferredNow, rec: &Record) -> io::Result<()> {
    write!(
        w,
        "[{} {:<5} {}:{} {}] {}",
        now.format("%Y-%m-%d %H:%M:%S%.3f"),
        rec.level(),
        rec.file().unwrap_or("unknown"),
        rec.line().unwrap_or(0),
        rec.target(),
        rec.args() // 文件直接输出原始内容
    )
}

pub fn init() -> flexi_logger::LoggerHandle {
    let logger_builder = Logger::try_with_env_or_str("info").unwrap();

    let file_cfg = FileSpec::default()
        .directory("logs")
        .basename("app")
        .suffix("log");

    logger_builder
        // 分别为控制台和文件指定格式化函数
        .format_for_stdout(format_with_color)
        .format_for_files(format_for_file)
        .log_to_file(file_cfg)
        .rotate(Criterion::Age(Age::Day), Naming::Timestamps, Cleanup::KeepLogFiles(7))
        .duplicate_to_stdout(Duplicate::All) 
        .start()
        .expect("日志初始化失败")
}