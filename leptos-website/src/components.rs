/*
 * Copyright 2025 Security Union LLC
 *
 * Licensed under either of
 *
 * * Apache License, Version 2.0
 *   (http://www.apache.org/licenses/LICENSE-2.0)
 * * MIT license
 *   (http://opensource.org/licenses/MIT)
 *
 * at your option.
 *
 * Unless you explicitly state otherwise, any contribution intentionally
 * submitted for inclusion in the work by you, as defined in the Apache-2.0
 * license, shall be dual licensed as above, without any additional terms or
 * conditions.
 */

pub mod CodeExample;
//pub mod DarkModeToggle;
pub mod CTAButton;
pub mod Footer;
pub mod HeroHeader;
pub mod Page;
pub mod SpeedStats;

// Section components
pub mod sections {
    pub mod Company;
    pub mod Customers;
    pub mod Developers;
    pub mod Pricing;
}

pub use CTAButton::*;
pub use CodeExample::*;
pub use Footer::*;
pub use HeroHeader::*;
pub use Page::*;
