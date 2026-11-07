// Modified by enzUI contributors. See FORK.md.
// Copyright 2026 The AccessKit Authors and enzUI contributors.
// Licensed under the Apache License, Version 2.0 or the MIT license.

use accesskit_atspi_common::PlatformNode;
use zbus::{fdo, interface};

use super::TextInterface;
use crate::virtual_text::{VirtualTextError, VirtualTextUnixConsumer};

/// AT-SPI EditableText for a registered lazy virtual document.
pub(crate) struct EditableTextInterface {
    node: PlatformNode,
}

impl EditableTextInterface {
    pub fn new(node: PlatformNode) -> Self {
        Self { node }
    }

    fn consumer(&self) -> fdo::Result<VirtualTextUnixConsumer> {
        VirtualTextUnixConsumer::for_platform_node(&self.node)
            .ok_or_else(|| fdo::Error::NotSupported("no lazy text provider".into()))
    }

    fn map(error: VirtualTextError) -> fdo::Error {
        TextInterface::map_virtual(error)
    }
}

#[interface(name = "org.a11y.atspi.EditableText")]
impl EditableTextInterface {
    fn copy_text(&self, _start_pos: i32, _end_pos: i32) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "clipboard ownership remains with the application".into(),
        ))
    }

    fn cut_text(&self, _start_pos: i32, _end_pos: i32) -> fdo::Result<bool> {
        Err(fdo::Error::NotSupported(
            "cut requires application clipboard ownership".into(),
        ))
    }

    fn delete_text(&self, start_pos: i32, end_pos: i32) -> fdo::Result<bool> {
        let consumer = self.consumer()?;
        let request = consumer.begin_request().map_err(Self::map)?;
        let summary = consumer.summary().map_err(Self::map)?;
        let start = u64::try_from(start_pos)
            .map_err(|_| fdo::Error::InvalidArgs("start offset is negative".into()))?;
        let end = u64::try_from(end_pos)
            .map_err(|_| fdo::Error::InvalidArgs("end offset is negative".into()))?;
        consumer
            .replace(summary, start..end, "", request.cancellation())
            .map_err(Self::map)?;
        Ok(true)
    }

    fn insert_text(&self, position: i32, text: &str, length: i32) -> fdo::Result<bool> {
        let consumer = self.consumer()?;
        let request = consumer.begin_request().map_err(Self::map)?;
        let summary = consumer.summary().map_err(Self::map)?;
        let position = u64::try_from(position)
            .map_err(|_| fdo::Error::InvalidArgs("position is negative".into()))?;
        let supplied = usize::try_from(length)
            .map_err(|_| fdo::Error::InvalidArgs("length is negative".into()))?;
        let end = supplied.min(text.len());
        let text = text
            .get(..end)
            .ok_or_else(|| fdo::Error::InvalidArgs("length splits a UTF-8 character".into()))?;
        consumer
            .replace(summary, position..position, text, request.cancellation())
            .map_err(Self::map)?;
        Ok(true)
    }

    fn paste_text(&self, _position: i32) -> fdo::Result<bool> {
        Err(fdo::Error::NotSupported(
            "paste requires application clipboard ownership".into(),
        ))
    }

    fn set_text_contents(&self, new_contents: &str) -> fdo::Result<bool> {
        let consumer = self.consumer()?;
        let request = consumer.begin_request().map_err(Self::map)?;
        let summary = consumer.summary().map_err(Self::map)?;
        consumer
            .replace(
                summary,
                0..summary.characters,
                new_contents,
                request.cancellation(),
            )
            .map_err(Self::map)?;
        Ok(true)
    }
}
