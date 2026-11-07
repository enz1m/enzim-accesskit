// Modified by enzUI contributors. See FORK.md.
// Copyright 2026 The AccessKit Authors and enzUI contributors.
// Licensed under the Apache License, Version 2.0 or the MIT license.

use accesskit_atspi_common::PlatformNode;
use zbus::{fdo, interface};

use super::TextInterface;
use crate::virtual_text::{VirtualTextError, VirtualTextUnixConsumer};

/// AT-SPI EditableText for a registered lazy virtual document, or (enzUI)
/// for an editable text input, whose edits replace its whole value through
/// the application's `SetValue` handling.
pub(crate) struct EditableTextInterface {
    node: PlatformNode,
}

impl EditableTextInterface {
    pub fn new(node: PlatformNode) -> Self {
        Self { node }
    }

    fn consumer(&self) -> Option<VirtualTextUnixConsumer> {
        VirtualTextUnixConsumer::for_platform_node(&self.node)
    }

    fn map(error: VirtualTextError) -> fdo::Error {
        TextInterface::map_virtual(error)
    }

    fn map_error(&self) -> impl '_ + FnOnce(accesskit_atspi_common::Error) -> fdo::Error {
        |error| crate::util::map_error_from_node(&self.node, error)
    }

    /// Replaces the characters `start..end` of an input's value with `text`
    /// and sets the result as its whole value.
    fn splice_native(&self, start: i32, end: i32, text: &str) -> fdo::Result<bool> {
        let value = self.node.text_contents().map_err(self.map_error())?;
        let chars = value.chars().count();
        let offset = |offset: i32| {
            usize::try_from(offset)
                .map(|offset| offset.min(chars))
                .map_err(|_| fdo::Error::InvalidArgs("offset is negative".into()))
        };
        let (start, end) = (offset(start)?, offset(end)?);
        if start > end {
            return Err(fdo::Error::InvalidArgs("start is after end".into()));
        }
        let byte = |char_offset: usize| {
            value
                .char_indices()
                .nth(char_offset)
                .map_or(value.len(), |(index, _)| index)
        };
        let mut next = String::with_capacity(value.len() + text.len());
        next.push_str(&value[..byte(start)]);
        next.push_str(text);
        next.push_str(&value[byte(end)..]);
        self.node
            .set_text_contents(&next)
            .map_err(self.map_error())?;
        Ok(true)
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
        let Some(consumer) = self.consumer() else {
            return self.splice_native(start_pos, end_pos, "");
        };
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
        let supplied = usize::try_from(length)
            .map_err(|_| fdo::Error::InvalidArgs("length is negative".into()))?;
        let end = supplied.min(text.len());
        let text = text
            .get(..end)
            .ok_or_else(|| fdo::Error::InvalidArgs("length splits a UTF-8 character".into()))?;
        let Some(consumer) = self.consumer() else {
            return self.splice_native(position, position, text);
        };
        let request = consumer.begin_request().map_err(Self::map)?;
        let summary = consumer.summary().map_err(Self::map)?;
        let position = u64::try_from(position)
            .map_err(|_| fdo::Error::InvalidArgs("position is negative".into()))?;
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
        let Some(consumer) = self.consumer() else {
            self.node
                .set_text_contents(new_contents)
                .map_err(self.map_error())?;
            return Ok(true);
        };
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
