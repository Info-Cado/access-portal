//! This computer's local date and time (the time zone set in Windows).

/// Local wall-clock time.
#[derive(Clone, Copy, Debug)]
pub struct LocalTime {
    pub year: u16,
    pub month: u16,
    /// 0 = Sunday ... 6 = Saturday.
    pub weekday: u8,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

impl LocalTime {
    /// Minutes since midnight.
    pub fn minutes(&self) -> u32 {
        u32::from(self.hour) * 60 + u32::from(self.minute)
    }
}

pub fn now() -> LocalTime {
    #[cfg(windows)]
    {
        #[repr(C)]
        struct SystemTime {
            year: u16,
            month: u16,
            day_of_week: u16,
            day: u16,
            hour: u16,
            minute: u16,
            second: u16,
            milliseconds: u16,
        }
        extern "system" {
            fn GetLocalTime(t: *mut SystemTime);
        }
        let mut t = SystemTime { year: 0, month: 0, day_of_week: 0, day: 0, hour: 0, minute: 0, second: 0, milliseconds: 0 };
        unsafe { GetLocalTime(&mut t) };
        LocalTime {
            year: t.year,
            month: t.month,
            weekday: t.day_of_week as u8,
            day: t.day,
            hour: t.hour,
            minute: t.minute,
            second: t.second,
        }
    }
    #[cfg(not(windows))]
    {
        // UTC elsewhere (the portal is meant to run on Windows).
        let secs = crate::db::now().max(0) as u64;
        let days = secs / 86_400;
        let rem = secs % 86_400;
        LocalTime {
            year: 1970,
            month: 1,
            weekday: ((days + 4) % 7) as u8, // 1970-01-01 was a Thursday
            day: 1,
            hour: (rem / 3600) as u16,
            minute: (rem % 3600 / 60) as u16,
            second: (rem % 60) as u16,
        }
    }
}
