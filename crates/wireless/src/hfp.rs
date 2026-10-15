use std::io::{self, Read, Write};

use crate::{Error, Result};

pub const HANDS_FREE_UUID: &str = "0000111e-0000-1000-8000-00805f9b34fb";
pub const AUDIO_GATEWAY_UUID: &str = "0000111f-0000-1000-8000-00805f9b34fb";

const HF_SUPPORTED_FEATURES: u32 = (1 << 2) | (1 << 3) | (1 << 4);
const MAX_LINE_LEN: usize = 512;

const POST_SLC_COMMANDS: [&str; 4] = ["AT+CLIP=1", "AT+CCWA=1", "AT+CMEE=1", "AT+CLCC"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Features,
    IndicatorMap,
    IndicatorValues,
    EventReporting,
    Established,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandsFreeEvent {
    Established,
    Battery { level: u8 },
}

#[derive(Debug)]
pub struct HandsFree {
    step: Step,
    awaiting_ok: bool,
    indicators: Vec<String>,
    post_slc: std::collections::VecDeque<&'static str>,
    line: Vec<u8>,
}

impl Default for HandsFree {
    fn default() -> Self {
        Self::new()
    }
}

impl HandsFree {
    pub fn new() -> Self {
        Self {
            step: Step::Features,
            awaiting_ok: false,
            indicators: Vec::new(),
            post_slc: POST_SLC_COMMANDS.into_iter().collect(),
            line: Vec::new(),
        }
    }

    pub const fn is_established(&self) -> bool {
        matches!(self.step, Step::Established)
    }

    pub fn start(&mut self) -> Vec<String> {
        if self.awaiting_ok || self.step != Step::Features {
            return Vec::new();
        }
        self.awaiting_ok = true;
        vec![command(&format!("AT+BRSF={HF_SUPPORTED_FEATURES}"))]
    }

    pub fn receive(&mut self, bytes: &[u8]) -> Result<(Vec<String>, Vec<HandsFreeEvent>)> {
        let mut outgoing = Vec::new();
        let mut events = Vec::new();
        for &byte in bytes {
            if byte == b'\r' || byte == b'\n' {
                if !self.line.is_empty() {
                    let line = String::from_utf8_lossy(&self.line).trim().to_owned();
                    self.line.clear();
                    self.handle_line(&line, &mut outgoing, &mut events);
                }
                continue;
            }
            if self.line.len() >= MAX_LINE_LEN {
                return Err(Error::Protocol("HFP line exceeds the maximum length"));
            }
            self.line.push(byte);
        }
        Ok((outgoing, events))
    }

    fn handle_line(
        &mut self,
        line: &str,
        outgoing: &mut Vec<String>,
        events: &mut Vec<HandsFreeEvent>,
    ) {
        if line == "OK" || line == "ERROR" || line.starts_with("+CME ERROR") {
            self.awaiting_ok = false;
            self.advance(outgoing, events);
            return;
        }
        if let Some(values) = line.strip_prefix("+CIND:") {
            let values = values.trim();
            if values.starts_with('(') {
                self.indicators = values
                    .split('"')
                    .skip(1)
                    .step_by(2)
                    .map(str::to_owned)
                    .collect();
            } else {
                self.report_indicators(values, events);
            }
            return;
        }
        if let Some(event) = line.strip_prefix("+CIEV:") {
            let mut fields = event.split(',').map(str::trim);
            if let (Some(Ok(index)), Some(Ok(value))) = (
                fields.next().map(str::parse::<usize>),
                fields.next().map(str::parse::<u8>),
            ) && self
                .indicators
                .get(index.wrapping_sub(1))
                .is_some_and(|name| name == "battchg")
            {
                events.push(HandsFreeEvent::Battery { level: value });
            }
        }
    }

    fn report_indicators(&self, values: &str, events: &mut Vec<HandsFreeEvent>) {
        for (name, value) in self.indicators.iter().zip(values.split(',')) {
            if name == "battchg"
                && let Ok(level) = value.trim().parse()
            {
                events.push(HandsFreeEvent::Battery { level });
            }
        }
    }

    fn advance(&mut self, outgoing: &mut Vec<String>, events: &mut Vec<HandsFreeEvent>) {
        let next = match self.step {
            Step::Features => {
                self.step = Step::IndicatorMap;
                Some("AT+CIND=?")
            }
            Step::IndicatorMap => {
                self.step = Step::IndicatorValues;
                Some("AT+CIND?")
            }
            Step::IndicatorValues => {
                self.step = Step::EventReporting;
                Some("AT+CMER=3,0,0,1")
            }
            Step::EventReporting => {
                self.step = Step::Established;
                events.push(HandsFreeEvent::Established);
                self.post_slc.pop_front()
            }
            Step::Established => self.post_slc.pop_front(),
        };
        if let Some(next) = next {
            self.awaiting_ok = true;
            outgoing.push(command(next));
        }
    }
}

fn command(text: &str) -> String {
    format!("{text}\r")
}

pub fn run_hands_free<S: Read + Write>(
    stream: &mut S,
    cancelled: impl Fn() -> bool,
    mut on_event: impl FnMut(HandsFreeEvent),
) -> Result<()> {
    let mut hands_free = HandsFree::new();
    write_lines(stream, hands_free.start())?;
    let mut buffer = [0_u8; 512];
    while !cancelled() {
        let read = match stream.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(Error::Io(error)),
        };
        let (outgoing, events) = hands_free.receive(&buffer[..read])?;
        write_lines(stream, outgoing)?;
        events.into_iter().for_each(&mut on_event);
    }
    Ok(())
}

fn write_lines<S: Write>(stream: &mut S, lines: Vec<String>) -> Result<()> {
    for line in lines {
        stream.write_all(line.as_bytes())?;
    }
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(hands_free: &mut HandsFree, text: &str) -> (Vec<String>, Vec<HandsFreeEvent>) {
        hands_free.receive(text.as_bytes()).expect("receive")
    }

    #[test]
    fn establishes_the_service_level_connection() {
        let mut hf = HandsFree::new();
        assert_eq!(hf.start(), vec!["AT+BRSF=28\r".to_owned()]);

        let (out, _) = feed(&mut hf, "\r\n+BRSF: 871\r\n\r\nOK\r\n");
        assert_eq!(out, vec!["AT+CIND=?\r".to_owned()]);

        let (out, _) = feed(
            &mut hf,
            "\r\n+CIND: (\"call\",(0,1)),(\"callsetup\",(0-3)),(\"service\",(0,1)),(\"signal\",(0-5)),(\"roam\",(0,1)),(\"battchg\",(0-5)),(\"callheld\",(0-2))\r\n\r\nOK\r\n",
        );
        assert_eq!(out, vec!["AT+CIND?\r".to_owned()]);

        let (out, events) = feed(&mut hf, "\r\n+CIND: 0,0,1,4,0,3,0\r\n\r\nOK\r\n");
        assert_eq!(out, vec!["AT+CMER=3,0,0,1\r".to_owned()]);
        assert_eq!(events, vec![HandsFreeEvent::Battery { level: 3 }]);
        assert!(!hf.is_established());

        let (out, events) = feed(&mut hf, "\r\nOK\r\n");
        assert_eq!(events, vec![HandsFreeEvent::Established]);
        assert_eq!(out, vec!["AT+CLIP=1\r".to_owned()]);
        assert!(hf.is_established());

        for expected in ["AT+CCWA=1\r", "AT+CMEE=1\r", "AT+CLCC\r"] {
            let (out, _) = feed(&mut hf, "\r\nOK\r\n");
            assert_eq!(out, vec![expected.to_owned()]);
        }
        let (out, _) = feed(&mut hf, "\r\nOK\r\n");
        assert!(out.is_empty());
    }

    #[test]
    fn errors_still_advance_the_dialogue() {
        let mut hf = HandsFree::new();
        hf.start();
        let (out, _) = feed(&mut hf, "\r\nERROR\r\n");
        assert_eq!(out, vec!["AT+CIND=?\r".to_owned()]);
    }

    #[test]
    fn battery_indicator_events_follow_the_announced_order() {
        let mut hf = HandsFree::new();
        hf.start();
        feed(&mut hf, "\r\nOK\r\n");
        feed(
            &mut hf,
            "\r\n+CIND: (\"service\",(0,1)),(\"battchg\",(0-5))\r\n",
        );
        let (_, events) = feed(&mut hf, "\r\n+CIEV: 2,5\r\n");
        assert_eq!(events, vec![HandsFreeEvent::Battery { level: 5 }]);
    }

    #[test]
    fn split_lines_are_reassembled() {
        let mut hf = HandsFree::new();
        hf.start();
        let (out, _) = feed(&mut hf, "\r\nO");
        assert!(out.is_empty());
        let (out, _) = feed(&mut hf, "K\r\n");
        assert_eq!(out, vec!["AT+CIND=?\r".to_owned()]);
    }

    #[test]
    fn runaway_lines_are_rejected() {
        let mut hf = HandsFree::new();
        assert!(hf.receive(&[b'x'; MAX_LINE_LEN + 1]).is_err());
    }
}
