/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-SEL
 */

#[macro_export]
macro_rules! location {
    () => {{ concat!(file!(), ":", line!()) }};
}

#[macro_export]
macro_rules! bail {
    ($err:expr $(,)?) => {
        return Err($err);
    };
}

#[macro_export]
macro_rules! error {
    ($err:expr $(,)?) => {
        let err = $err;
        let event_id = err.as_ref().id();

        if $crate::Collector::is_metric(event_id) {
            $crate::Collector::record_metric(*err.as_ref(), event_id, err.keys());
        }
        if $crate::Collector::has_interest(event_id) {
            err.send();
        }
    };
}
