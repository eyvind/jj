// Copyright 2020 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::io::{Stderr, Stdout, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::str::FromStr;
use std::{fmt, io, mem};

use crossterm::tty::IsTty;
use jujutsu_lib::settings::UserSettings;

use crate::config::FullCommandArgs;
use crate::formatter::{Formatter, FormatterFactory};

pub struct Ui {
    color: bool,
    pager_cmd: FullCommandArgs,
    paginate: PaginationChoice,
    progress_indicator: bool,
    formatter_factory: FormatterFactory,
    output: UiOutput,
    settings: UserSettings,
}

fn progress_indicator_setting(settings: &UserSettings) -> bool {
    settings
        .config()
        .get_bool("ui.progress-indicator")
        .unwrap_or(true)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ColorChoice {
    Always,
    Never,
    Auto,
}

impl Default for ColorChoice {
    fn default() -> Self {
        ColorChoice::Auto
    }
}

impl FromStr for ColorChoice {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "always" => Ok(ColorChoice::Always),
            "never" => Ok(ColorChoice::Never),
            "auto" => Ok(ColorChoice::Auto),
            _ => Err("must be one of always, never, or auto"),
        }
    }
}

impl fmt::Display for ColorChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ColorChoice::Always => "always",
            ColorChoice::Never => "never",
            ColorChoice::Auto => "auto",
        };
        write!(f, "{s}")
    }
}

fn color_setting(settings: &UserSettings) -> ColorChoice {
    settings
        .config()
        .get_string("ui.color")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_default()
}

fn use_color(choice: ColorChoice) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => io::stdout().is_tty(),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PaginationChoice {
    No,
    Auto,
}

impl Default for PaginationChoice {
    fn default() -> Self {
        PaginationChoice::Auto
    }
}

fn pager_setting(settings: &UserSettings) -> FullCommandArgs {
    settings
        .config()
        .get("ui.pager")
        .unwrap_or_else(|_| "less -FRX".into())
}

impl Default for Ui {
    fn default() -> Self {
        Self::new()
    }
}

impl Ui {
    pub fn new() -> Ui {
        let settings = UserSettings::from_config(crate::config::default_config());
        let color = use_color(color_setting(&settings));
        let progress_indicator = progress_indicator_setting(&settings);
        let formatter_factory = FormatterFactory::prepare(&settings, color);
        Ui {
            color,
            formatter_factory,
            pager_cmd: pager_setting(&settings),
            paginate: PaginationChoice::Auto,
            progress_indicator,
            output: UiOutput::new_terminal(),
            settings,
        }
    }

    pub fn reset(&mut self, settings: UserSettings) {
        // TODO: maybe Ui shouldn't take ownership of UserSettings
        self.color = use_color(color_setting(&settings));
        self.pager_cmd = pager_setting(&settings);
        self.progress_indicator = progress_indicator_setting(&settings);
        self.formatter_factory = FormatterFactory::prepare(&settings, self.color);
        self.settings = settings;
    }

    /// Sets the pagination value.
    pub fn set_pagination(&mut self, choice: PaginationChoice) {
        self.paginate = choice;
    }

    /// Switches the output to use the pager, if allowed.
    pub fn request_pager(&mut self) {
        if self.paginate == PaginationChoice::No {
            return;
        }

        match self.output {
            UiOutput::Terminal { .. } if io::stdout().is_tty() => {
                match UiOutput::new_paged(&self.pager_cmd) {
                    Ok(new_output) => {
                        self.output = new_output;
                    }
                    Err(e) => {
                        self.write_warn(&format!("Failed to spawn pager: {e}\n"))
                            .ok();
                    }
                }
            }
            UiOutput::Terminal { .. } | UiOutput::Paged { .. } => {}
        }
    }

    pub fn color(&self) -> bool {
        self.color
    }

    pub fn settings(&self) -> &UserSettings {
        &self.settings
    }

    pub fn new_formatter<'output, W: Write + 'output>(
        &self,
        output: W,
    ) -> Box<dyn Formatter + 'output> {
        self.formatter_factory.new_formatter(output)
    }

    /// Creates a formatter for the locked stdout stream.
    ///
    /// Labels added to the returned formatter should be removed by caller.
    /// Otherwise the last color would persist.
    pub fn stdout_formatter<'a>(&'a self) -> Box<dyn Formatter + 'a> {
        match &self.output {
            UiOutput::Terminal { stdout, .. } => self.new_formatter(stdout.lock()),
            UiOutput::Paged { child_stdin, .. } => self.new_formatter(child_stdin),
        }
    }

    /// Creates a formatter for the locked stderr stream.
    pub fn stderr_formatter<'a>(&'a self) -> Box<dyn Formatter + 'a> {
        match &self.output {
            UiOutput::Terminal { stderr, .. } => self.new_formatter(stderr.lock()),
            UiOutput::Paged { child_stdin, .. } => self.new_formatter(child_stdin),
        }
    }

    /// Whether continuous feedback should be displayed for long-running
    /// operations
    pub fn use_progress_indicator(&self) -> bool {
        self.progress_indicator && io::stdout().is_tty()
    }

    pub fn write(&mut self, text: &str) -> io::Result<()> {
        let data = text.as_bytes();
        match &mut self.output {
            UiOutput::Terminal { stdout, .. } => stdout.write_all(data),
            UiOutput::Paged { child_stdin, .. } => child_stdin.write_all(data),
        }
    }

    pub fn write_stderr(&mut self, text: &str) -> io::Result<()> {
        let data = text.as_bytes();
        match &mut self.output {
            UiOutput::Terminal { stderr, .. } => stderr.write_all(data),
            UiOutput::Paged { child_stdin, .. } => child_stdin.write_all(data),
        }
    }

    pub fn write_fmt(&mut self, fmt: fmt::Arguments<'_>) -> io::Result<()> {
        match &mut self.output {
            UiOutput::Terminal { stdout, .. } => stdout.write_fmt(fmt),
            UiOutput::Paged { child_stdin, .. } => child_stdin.write_fmt(fmt),
        }
    }

    pub fn write_hint(&mut self, text: impl AsRef<str>) -> io::Result<()> {
        let mut formatter = self.stderr_formatter();
        formatter.add_label("hint")?;
        formatter.write_str(text.as_ref())?;
        formatter.remove_label()?;
        Ok(())
    }

    pub fn write_warn(&mut self, text: impl AsRef<str>) -> io::Result<()> {
        let mut formatter = self.stderr_formatter();
        formatter.add_label("warning")?;
        formatter.write_str(text.as_ref())?;
        formatter.remove_label()?;
        Ok(())
    }

    pub fn write_error(&mut self, text: &str) -> io::Result<()> {
        let mut formatter = self.stderr_formatter();
        formatter.add_label("error")?;
        formatter.write_str(text)?;
        formatter.remove_label()?;
        Ok(())
    }

    pub fn flush(&mut self) -> io::Result<()> {
        match &mut self.output {
            UiOutput::Terminal { stdout, .. } => stdout.flush(),
            UiOutput::Paged { child_stdin, .. } => child_stdin.flush(),
        }
    }

    pub fn finalize_writes(&mut self) {
        if let UiOutput::Paged {
            mut child,
            child_stdin,
        } = mem::replace(&mut self.output, UiOutput::new_terminal())
        {
            drop(child_stdin);
            if let Err(e) = child.wait() {
                // It's possible (though unlikely) that this write fails, but
                // this function gets called so late that there's not much we
                // can do about it.
                self.write_error(&format!("Failed to wait on pager: {e}\n"))
                    .ok();
            }
        }
    }

    pub fn prompt(&mut self, prompt: &str) -> io::Result<String> {
        if !io::stdout().is_tty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Cannot prompt for input since the output is not connected to a terminal",
            ));
        }
        write!(self, "{prompt}: ")?;
        self.flush()?;
        let mut buf = String::new();
        io::stdin().read_line(&mut buf)?;
        Ok(buf)
    }

    pub fn prompt_password(&mut self, prompt: &str) -> io::Result<String> {
        if !io::stdout().is_tty() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Cannot prompt for input since the output is not connected to a terminal",
            ));
        }
        rpassword::prompt_password(format!("{prompt}: "))
    }

    pub fn size(&self) -> Option<(u16, u16)> {
        crossterm::terminal::size().ok()
    }

    /// Construct a guard object which writes `data` when dropped. Useful for
    /// restoring terminal state.
    pub fn output_guard(&self, text: String) -> OutputGuard {
        OutputGuard {
            text,
            output: match self.output {
                UiOutput::Terminal { .. } => io::stdout(),
                // TODO we don't actually need to write in this case, so it
                // might be better to no-op
                UiOutput::Paged { .. } => io::stdout(),
            },
        }
    }
}

enum UiOutput {
    Terminal {
        stdout: Stdout,
        stderr: Stderr,
    },
    Paged {
        child: Child,
        child_stdin: ChildStdin,
    },
}

impl UiOutput {
    fn new_terminal() -> UiOutput {
        UiOutput::Terminal {
            stdout: io::stdout(),
            stderr: io::stderr(),
        }
    }

    fn new_paged(pager_cmd: &FullCommandArgs) -> io::Result<UiOutput> {
        let mut child = pager_cmd.to_command().stdin(Stdio::piped()).spawn()?;
        let child_stdin = child.stdin.take().unwrap();
        Ok(UiOutput::Paged { child, child_stdin })
    }
}

pub struct OutputGuard {
    text: String,
    output: Stdout,
}

impl Drop for OutputGuard {
    fn drop(&mut self) {
        _ = self.output.write_all(self.text.as_bytes());
    }
}
