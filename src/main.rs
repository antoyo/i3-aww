use std::{io, time::Duration, process::Command, sync::Arc};

use dashmap::DashMap;
use glib::{MainLoop, timeout_add_once};
use gudev::{Client, traits::{ClientExt, DeviceExt}};
use i3_ipc::{
    event::{Event, Subscribe},
    I3Stream, msg::Msg, I3, Connect,
};
use xrandr::XHandle;

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

fn monitor_connected(name: &str) -> bool {
    let mut handle = XHandle::open().unwrap();
    let outputs = handle.all_outputs().unwrap();
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
        eprintln!("Cannot move workspace: {}", error);
    }
}

fn main() -> io::Result<()> {
    // TODO: instead of taking those as cli arguments, infer them from the current xrandr config.
    let primary_monitor = "HDMI-1".to_string();
    let monitor_pos = "DVI-D-1:--right-of HDMI-1";

    let monitor_pos = MonitorPos::parse(monitor_pos);

    let workspaces = Arc::new(DashMap::new());

    let mut i3 = I3Stream::conn_sub(&[Subscribe::Window, Subscribe::Workspace]).unwrap();
    if let Ok(i3_workspaces) = i3.get_workspaces() {
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
            let mut i3 = I3::connect().unwrap();
            if let Ok(i3_workspaces) = i3.get_workspaces() {
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
                let mut i3 = I3::connect().unwrap();
                if let Ok(i3_workspaces) = i3.get_workspaces() {
                    for workspace in &i3_workspaces {
                        existing_workspaces.push(workspace.num);
                    }
                }

                let focused_workspace = get_focused_workspace(&mut i3);

                let mut handle = XHandle::open().unwrap();
                let outputs = handle.all_outputs().unwrap();
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
                    let mut i3 = I3::connect().unwrap();

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
