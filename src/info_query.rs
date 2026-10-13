//! Host-provided data for the 2022 five information queries
//! (A.2.4.10-14 → A.2.6.12-16). Every method returns `None` by
//! default — the minimal valid answer (empty optional block) applies;
//! install a provider to answer with real hardware state.

/// A.2.6.12 看守位信息查询应答 body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomePositionInfo {
    /// 0-关闭；1-开启.
    pub enabled: i32,
    /// 自动归位时间间隔（秒）.
    pub reset_time_secs: Option<i32>,
    /// 调用预置位编号 0..=255.
    pub preset_index: Option<i32>,
}

/// A.2.6.13 巡航轨迹列表 entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CruiseTrackInfo {
    /// 轨迹编号（0 = 第一条）.
    pub number: i32,
    /// 轨迹名称（≤32 字节）.
    pub name: String,
}

/// A.2.6.14 巡航轨迹查询应答 body: the track and its points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CruiseTrackDetail {
    pub number: i32,
    pub name: String,
    pub points: Vec<CruisePointInfo>,
}

/// One A.2.6.14 轨迹点.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CruisePointInfo {
    pub preset_index: i32,
    /// 停留时间（秒）.
    pub stay_time_secs: i32,
    /// 云台速度 1..=15.
    pub speed: i32,
}

/// A.2.6.15 PTZ 精确状态查询应答 body (every field optional per the
/// schema).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PtzPositionInfo {
    pub pan: Option<f64>,
    pub tilt: Option<f64>,
    pub zoom: Option<f64>,
    /// 摄像机水平视场角.
    pub horizontal_field_angle: Option<f64>,
    /// 摄像机竖直视场角.
    pub vertical_field_angle: Option<f64>,
    /// 摄像机可视距离.
    pub max_view_distance: Option<f64>,
}

/// One A.2.6.16 存储卡 entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SdCardInfo {
    /// SD 卡编号.
    pub id: i32,
    pub hdd_name: String,
    /// `ok` / `formatting` / `unformatted` / `idle` / `error`.
    pub status: String,
    /// 格式化进度 0-100 (%).
    pub format_progress: Option<i32>,
    /// 存储容量（MB）.
    pub capacity_mb: i64,
    /// 剩余容量（MB）.
    pub free_space_mb: i64,
}

/// The host seam for the 2022 five information queries. All methods
/// default to `None` (the minimal valid answer).
pub trait InfoQueryProvider: Send + Sync {
    /// A.2.6.12 HomePositionQuery.
    fn home_position(&self) -> Option<HomePositionInfo> {
        None
    }
    /// A.2.6.13 CruiseTrackListQuery.
    fn cruise_track_list(&self) -> Option<Vec<CruiseTrackInfo>> {
        None
    }
    /// A.2.6.14 CruiseTrackQuery for the given track number.
    fn cruise_track(&self, _number: i32) -> Option<CruiseTrackDetail> {
        None
    }
    /// A.2.6.15 PTZPosition.
    fn ptz_position(&self) -> Option<PtzPositionInfo> {
        None
    }
    /// A.2.6.16 SDCardStatus (≤ 8 items per the schema).
    fn sd_card_status(&self) -> Option<Vec<SdCardInfo>> {
        None
    }
}

/// The all-`None` default — identical answers to no provider installed.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoInfoQueries;

impl InfoQueryProvider for NoInfoQueries {}
