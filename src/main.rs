/*
 * FIXME: it doesn't always keep the focused (not only visible) workspace focused and visible when
 * disconnecting a monitor.
 * FIXME: uses 100% CPU (seems to happen when having multiple instances of i3-aww running).
 * FIXME: if a workspace is empty, it won't be put back on the correct monitor.
 * TODO: reset mouse position when plugging back the second monitor.
 * TODO: if pressing on the active button on the KVM switch, it moves all the workspaces on one
 * screen (possibly because we don't handle the case where the config change to the same config).
 */

use std::{io, time::Duration, process::Command, sync::Arc};

use dashmap::DashMap;
use glib::{MainLoop, timeout_add_once};
use gudev::{Client, traits::{ClientExt, DeviceExt}};
use i3_ipc::{
    event::{Event, Subscribe},
    I3Stream, msg::Msg, I3, Connect,
};
use xrandr::{XHandle, Output};

struct MonitorData {
    name: String,
    connected: bool,
}

#[derive(Clone, Debug)]
struct MonitorPos {
    name: String,
    args: Vec<String>,
}

#[derive(Debug)]
struct Workspace {
    focused: bool,
    num: i32,
    output: String,
    previous_output: Option<String>,
    was_focused: bool,
}

impl MonitorPos {
    fn parse(data: &str) -> Option<Self> {
        let mut data = data.split(':');
        let name = data.next()?.to_string();
        let args_string = data.next()?.to_string();
        let args = args_string.split_ascii_whitespace()
            .map(|str| str.to_string())
            .collect();
        Some(Self {
            name,
            args,
        })
    }
}

fn xrandr_outputs() -> Vec<Output> {
    let outputs = (|| {
        let mut handle = XHandle::open()?;
        handle.all_outputs()
    })();
    outputs.unwrap_or(vec![])
}

fn monitor_connected(name: &str) -> bool {
    let outputs = xrandr_outputs();
    for output in outputs {
        if output.name == name {
            let connected = output.edid().is_some();
            if connected {
                return true;
            }
        }
    }
    false
}

fn get_focused_workspace(i3: &mut I3Stream) -> Option<i32> {
    if let Ok(i3_workspaces) = i3.get_workspaces() {
        for workspace in &i3_workspaces {
            if workspace.focused {
                return Some(workspace.num);
            }
        }
    }
    None
}

fn focus(i3: &mut I3Stream, num: i32) {
    let command = format!("workspace {}", num);
    if let Err(error) = i3.send_msg(Msg::RunCommand, &command) {
        eprintln!("Cannot focus workspace: {}", error);
    }
}

fn main() -> io::Result<()> {
    // TODO: instead of taking those as cli arguments, infer them from the current xrandr config.
    let primary_monitor = "HDMI-A-0".to_string();
    let monitor_pos = "DVI-D-0:--right-of HDMI-A-0";

    let monitor_pos = MonitorPos::parse(monitor_pos);

    let workspaces = Arc::new(DashMap::new());

    let i3 = I3::connect();
    if let Ok(i3_workspaces) = i3.and_then(|mut i3| i3.get_workspaces()) {
        for workspace in &i3_workspaces {
            let num = workspace.num;
            workspaces.insert(num, Workspace {
                focused: workspace.focused || workspace.visible,
                num,
                output: workspace.output.clone(),
                previous_output: None,
                was_focused: false,
            });
        }
    }

    let adjust_workspaces = {
        let workspaces = Arc::clone(&workspaces);
        move || {
            if let Ok(i3_workspaces) = I3::connect().and_then(|mut i3| i3.get_workspaces()) {
                for workspace in &i3_workspaces {
                    let num = workspace.num;

                    let mut previous_output = None;
                    let mut was_focused = false;
                    if let Some(old_workspace) = workspaces.get(&num) {
                        // If there was no change, keep the old data.
                        if workspace.output == old_workspace.output {
                            previous_output = old_workspace.previous_output.clone();
                            was_focused = old_workspace.was_focused;
                        }
                        // If there was a change after the monitor was disconnected.
                        else if !monitor_connected(&old_workspace.output) {
                            previous_output = Some(old_workspace.output.clone());
                            was_focused = old_workspace.focused;
                        }
                    }

                    let workspace = Workspace {
                        focused: workspace.focused || workspace.visible,
                        num,
                        output: workspace.output.clone(),
                        previous_output,
                        was_focused,
                    };
                    workspaces.insert(num, workspace);
                }
            }
        }
    };

    std::thread::spawn({
        let adjust_workspaces = adjust_workspaces.clone();
        move || {
            if let Ok(mut i3) = I3Stream::conn_sub(&[Subscribe::Window, Subscribe::Workspace]) {
                for event in i3.listen() {
                    if let Ok(event) = event {
                        match event {
                            Event::Workspace(_) => {
                                adjust_workspaces();
                            },
                            Event::Output(_) | Event::Window(_) | Event::Mode(_) | Event::BarConfig(_) | Event::Binding(_) |
                                Event::Shutdown(_) | Event::Tick(_) => (),
                        }
                    }
                }
            }
        }
    });

    let client = Client::new(&[]);

    client.connect_uevent(move |_client, _name, device| {
        if device.devtype().map(|string| string.to_string()) == Some("drm_minor".to_string()) {
            let primary_monitor = primary_monitor.clone();
            let monitor_pos = monitor_pos.clone();
            let workspaces = Arc::clone(&workspaces);
            let adjust_workspaces = adjust_workspaces.clone();
            timeout_add_once(Duration::from_millis(500), move || {
                // Since i3 creates empty workspaces, make a list of existing workspaces to avoid
                // focusing unexisting workspaces later.
                let mut existing_workspaces = vec![];
                let focused_workspace = {
                    if let Ok(mut i3) = I3::connect() {
                        if let Ok(i3_workspaces) = i3.get_workspaces() {
                            for workspace in &i3_workspaces {
                                existing_workspaces.push(workspace.num);
                            }
                        }

                        get_focused_workspace(&mut i3)
                    }
                    else {
                        None
                    }
                };

                let outputs = xrandr_outputs();
                let mut monitor_data = vec![];
                for output in outputs {
                    let connected = output.edid().is_some();
                    monitor_data.push(MonitorData {
                        name: output.name,
                        connected,
                    });
                }

                let mut command = Command::new("xrandr");

                let mut primary_connected = false;

                for monitor in &monitor_data {
                    if primary_monitor == monitor.name && monitor.connected {
                        primary_connected = true;
                    }
                }

                let mut primary_set = primary_connected;

                for monitor in &monitor_data {
                    command.arg("--output");
                    command.arg(&monitor.name);

                    if monitor.connected {
                        command.arg("--auto");

                        if let Some(ref monitor_pos) = monitor_pos {
                            if monitor_pos.name == monitor.name {
                                command.args(&monitor_pos.args);
                            }
                        }

                        if monitor.name == primary_monitor || !primary_set {
                            command.arg("--primary");
                            primary_set = true;
                        }
                    }
                    else {
                        command.arg("--off");
                    }
                }

                if let Err(error) = command.status() {
                    eprintln!("Could not set the monitor config: {}", error);
                }

                timeout_add_once(Duration::from_millis(500), move || {
                    adjust_workspaces();
                    let mut i3 =
                        match I3::connect() {
                            Ok(i3) => i3,
                            Err(error) => {
                                eprintln!("Error connecting to i3: {}", error);
                                return;
                            },
                        };

                    // Move the workspaces to their previous monitor.
                    for workspace in workspaces.iter() {
                        if let Some(ref output) = workspace.previous_output {
                            if monitor_connected(output) {
                                let command = format!("[workspace=\"{}\"] move workspace to output {}", workspace.num, output);
                                if let Err(error) = i3.send_msg(Msg::RunCommand, &command) {
                                    eprintln!("Cannot move workspace: {}", error);
                                }
                            }
                        }
                    }

                    // Make visible the right workspaces.
                    for workspace in workspaces.iter() {
                        if workspace.was_focused && existing_workspaces.contains(&workspace.num) {
                            focus(&mut i3, workspace.num);
                        }
                    }

                    if let Some(workspace) = focused_workspace {
                        if existing_workspaces.contains(&workspace) {
                            focus(&mut i3, workspace);
                        }
                    }
                });
            });
        }
    });

    let main_loop = MainLoop::new(None, false);
    main_loop.run();

    Ok(())
}
