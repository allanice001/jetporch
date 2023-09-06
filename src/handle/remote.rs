// Jetporch
// Copyright (C) 2023 - Michael DeHaan <michael@michaeldehaan.net> + contributors
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// at your option) any later version.
// 
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
// 
// You should have received a copy of the GNU General Public License
// long with this program.  If not, see <http://www.gnu.org/licenses/>.

use std::sync::{Arc,Mutex,RwLock};
use std::path::Path;
use crate::connection::connection::Connection;
use crate::connection::command::cmd_info;
use crate::tasks::request::{TaskRequest, TaskRequestType};
use crate::tasks::response::TaskResponse;
use crate::inventory::hosts::{Host,HostOSType};
use crate::playbooks::traversal::RunState;
use crate::tasks::fields::Field;
use crate::tasks::FileAttributesEvaluated;
use crate::tasks::cmd_library::screen_general_input_loose;
use crate::handle::handle::CheckRc;
use crate::handle::template::Safety;
use crate::handle::response::Response;
use crate::handle::template::Template;
use crate::tasks::files::Recurse;
use std::path::PathBuf;
use guid_create::GUID;

pub struct Remote {
    _run_state: Arc<RunState>, 
    connection: Arc<Mutex<dyn Connection>>,
    host: Arc<RwLock<Host>>, 
    template: Arc<Template>,
    response: Arc<Response>
}

#[derive(Debug,Copy,Clone,PartialEq)]
pub enum UseSudo {
    Yes,
    No
}

impl Remote {

    pub fn new(
        run_state_handle: Arc<RunState>, 
        connection_handle: Arc<Mutex<dyn Connection>>, 
        host_handle: Arc<RwLock<Host>>, 
        template_handle: Arc<Template>,
        response: Arc<Response>) -> Self {
        
        Self {
            _run_state: run_state_handle,
            connection: connection_handle,
            host: host_handle,
            template: template_handle,
            response: response,
        }
    }

    fn unwrap_string_result(&self, request: &Arc<TaskRequest>, str_result: &Result<String,String>) -> Result<String, Arc<TaskResponse>> {
        return match str_result {
            Ok(x) => Ok(x.clone()),
            Err(y) => {
                return Err(self.response.is_failed(request, &y.clone()));
            }
        };
    }

    pub fn get_whoami(&self) -> Result<String,String> {
        return self.connection.lock().unwrap().whoami();
    }

    pub fn make_temp_path(&self, who: &String, request: &Arc<TaskRequest>) -> Result<(PathBuf, PathBuf), Arc<TaskResponse>> {
        let mut pb = PathBuf::new();
        // slight buglet, if the user has no homedir access in this location it this will not work, but it seemed better than trusting $HOME
        // users can make a symlink in the worst case

        let tmpdir = match who.eq("root") {
            false => format!("/home/{}/.jet/tmp", who),
            true => String::from("/root/.jet/tmp")
        };
        pb.push(tmpdir);
        let mut pb2 = pb.clone();
        let guid = GUID::rand().to_string();
        pb2.push(guid.as_str());
        let create_tmp_dir = format!("mkdir -p '{}'", pb.display());
        self.run_no_sudo(request, &create_tmp_dir, CheckRc::Checked)?;
        return Ok((pb.clone(), pb2.clone()));
    }

    pub fn run(&self, request: &Arc<TaskRequest>, cmd: &String, check_rc: CheckRc) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        return self.internal_run(request, cmd, Safety::Safe, check_rc, UseSudo::Yes);
    }

    pub fn run_no_sudo(&self, request: &Arc<TaskRequest>, cmd: &String, check_rc: CheckRc) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        return self.internal_run(request, cmd, Safety::Safe, check_rc, UseSudo::No);
    }

    pub fn run_unsafe(&self, request: &Arc<TaskRequest>, cmd: &String, check_rc: CheckRc) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        return self.internal_run(request, cmd, Safety::Unsafe, check_rc, UseSudo::Yes);
    }

    fn internal_run(&self, request: &Arc<TaskRequest>, cmd: &String, safe: Safety, check_rc: CheckRc, use_sudo: UseSudo) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        assert!(request.request_type != TaskRequestType::Validate, "commands cannot be run in validate stage");
        // apply basic screening of the entire shell command, more filtering should already be done by cmd_library
        // for parameterized calls that use that
                
        if safe == Safety::Safe {
            match screen_general_input_loose(&cmd) {
                Ok(_x) => {},
                Err(y) => return Err(self.response.is_failed(request, &y.clone()))
            }
        }

        let cmd_out = match use_sudo {
            UseSudo::Yes => match self.template.add_sudo_details(request, &cmd) {
                Ok(x) => x,
                Err(y) => { return Err(self.response.is_failed(request, &format!("failure constructing sudo command: {}", y))); }
            },
            UseSudo::No => cmd.clone() 
        };

        let result = self.connection.lock().unwrap().run_command(&self.response, request, &cmd_out);

        // FIXME: this is reused below, move into function (see run_local)
        if check_rc == CheckRc::Checked {
            if result.is_ok() {
                let ok_result = result.as_ref().unwrap();
                let cmd_result = ok_result.command_result.as_ref().as_ref().unwrap();
                if cmd_result.rc != 0 {
                    // FIXME: since cmd_result is cloneable there is no need for it to be in an Arc
                    return Err(self.response.command_failed(request, &Arc::new(Some(cmd_result.clone()))));
                }
            }
        }

        return result;
    }

    pub fn get_os_type(&self) -> HostOSType {
        let os_type = self.host.read().unwrap().os_type;
        if os_type.is_none() {
            panic!("failed to detect OS type for {}, bailing out", self.host.read().unwrap().name);
        }
        return os_type.unwrap();
    }

    fn get_transfer_location(&self, request: &Arc<TaskRequest>, path: &String) -> Result<(Option<PathBuf>, Option<PathBuf>), Arc<TaskResponse>> {
        let os_type = self.get_os_type();
        let check_cmd1 = match crate::tasks::cmd_library::get_group_owned_command(os_type, &path) {
            Ok(x) => x,
            Err(y) => { return Err(self.response.is_failed(request, &format!("failed getting group ownership command: {}", y))); }
        };
        let check_result1 = self.run(request, &check_cmd1, CheckRc::Unchecked);
        if check_result1.is_err() {
            return Err(check_result1.unwrap_err());
        }
        let (mut rc, mut _out) = cmd_info(&check_result1.unwrap());
    
        if rc != 0 {
            let check_cmd2 = match crate::tasks::cmd_library::get_user_owned_command(os_type, &path) {
                Ok(x) => x,
                Err(y) => { return Err(self.response.is_failed(request, &format!("failed getting user ownership command: {}", y))); }
            };
            let check_result2 = self.run(request, &check_cmd2, CheckRc::Unchecked);
            if check_result2.is_err() {
                return Err(check_result2.unwrap_err());
            }
            (rc, _out) = cmd_info(&check_result2.unwrap());
        }
        if rc == 0 {
            return Ok((None, None));
        }
        let whoami = match self.get_whoami() {
            Ok(x) => x, Err(y) =>  { return Err(self.response.is_failed(request, &format!("failed checking current user: {}", y))); }
        };
        let (p1,f1) = self.make_temp_path(&whoami, request)?;
        return Ok((Some(p1.clone()), Some(f1.clone())))
    }

    fn get_effective_filename(&self, temp_dir: Option<PathBuf>, temp_path: Option<PathBuf>, path: &String) -> String {
        return match temp_dir.is_some() {
            true => {
                let t = temp_path.as_ref().unwrap();
                t.clone().into_os_string().into_string().unwrap()
            },
            false =>  path.clone()
        };
    }

    fn conditionally_move_back(&self, request: &Arc<TaskRequest>, temp_path: Option<PathBuf>, temp_dir: Option<PathBuf>, desired_path: &String) -> Result<(), Arc<TaskResponse>> {
        if temp_dir.is_some() {
            let move_to_correct_location = format!("mv '{}' '{}'", temp_path.as_ref().unwrap().display(), desired_path);
            let delete_tmp_location = format!("rm '{}'", temp_path.as_ref().unwrap().display());
            self.run(request, &move_to_correct_location, CheckRc::Checked)?;
        }
        Ok(())
    }

    // writes a string (for example, from a template) to a remote file location
    pub fn write_data<G>(&self, request: &Arc<TaskRequest>, data: &String, path: &String, mut before_complete: G) -> Result<(), Arc<TaskResponse>> 
        where G: FnMut(&String) -> Result<(), Arc<TaskResponse>> {   
        let (temp_dir, temp_path) = self.get_transfer_location(request, path)?;
        let real_path = self.get_effective_filename(temp_dir.clone(), temp_path.clone(), path); /* will be either temp_path or path */
        let xfer_result = self.connection.lock().unwrap().write_data(&self.response, request, data, &real_path)?;
        before_complete(&real_path.clone())?;
        self.conditionally_move_back(request, temp_dir.clone(), temp_path.clone(), path)?;
        return Ok(xfer_result);
    }

    pub fn copy_file<G>(&self, request: &Arc<TaskRequest>, src: &Path, dest: &String, mut before_complete: G) -> Result<(), Arc<TaskResponse>> 
    where G: FnMut(&String) -> Result<(), Arc<TaskResponse>> {   
        let (temp_dir, temp_path) = self.get_transfer_location(request, dest)?;
        let real_path = self.get_effective_filename(temp_dir.clone(), temp_path.clone(), dest); /* will be either temp_path or path */
        let xfer_result = self.connection.lock().unwrap().copy_file(&self.response, &request, src, &real_path)?;        
        before_complete(&real_path.clone())?;
        self.conditionally_move_back(request, temp_dir.clone(), temp_path.clone(), dest)?;
        return Ok(xfer_result);
    }

    pub fn get_mode(&self, request: &Arc<TaskRequest>, path: &String) -> Result<Option<String>,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::get_mode_command(self.get_os_type(), path);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        
        let result = self.run(request, &cmd, CheckRc::Unchecked)?;
        let (rc, out) = cmd_info(&result);
        return match rc {
            // we can all unwrap because all possible string lists will have at least 1 element
            0 => Ok(Some(out.split_whitespace().nth(0).unwrap().to_string())),
            _ => Ok(None),
        }
    }

    pub fn get_is_file(&self, request: &Arc<TaskRequest>, path: &String) -> Result<bool,Arc<TaskResponse>> {
        return match self.get_is_directory(request, path) {
            Ok(true) => Ok(false),
            Ok(false) => Ok(true),
            Err(x) => Err(x)
        };
    }

    pub fn get_is_directory(&self, request: &Arc<TaskRequest>, path: &String) -> Result<bool,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::get_is_directory_command(self.get_os_type(), path);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        
        let result = self.run(request, &cmd, CheckRc::Checked)?;
        let (_rc, out) = cmd_info(&result);
        // so far this assumes reliable ls -ld output across all supported operating systems, this may change
        // in wich case we may need to consider os_type here
        if out.starts_with("d") {
            return Ok(true);
        }
        return Ok(false);
    }

    pub fn touch_file(&self, request: &Arc<TaskRequest>, path: &String) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::get_touch_command(self.get_os_type(), path);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        return self.run(request, &cmd, CheckRc::Checked);  
    }

    pub fn create_directory(&self, request: &Arc<TaskRequest>, path: &String) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::get_create_directory_command(self.get_os_type(), path);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        return self.run(request, &cmd, CheckRc::Checked);  
    }

    pub fn delete_file(&self, request: &Arc<TaskRequest>, path: &String) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::get_delete_file_command(self.get_os_type(), path);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        return self.run(request, &cmd, CheckRc::Checked);  
    }

    pub fn delete_directory(&self, request: &Arc<TaskRequest>, path: &String, recurse: Recurse) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::get_delete_directory_command(self.get_os_type(), path, recurse);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        if path.eq("/") {
            return Err(self.response.is_failed(request, &String::from("accidental removal of / blocked by safeguard")));
        }
        return self.run(request, &cmd, CheckRc::Checked);  
    }

    pub fn get_ownership(&self, request: &Arc<TaskRequest>, path: &String) -> Result<Option<(String,String)>,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::get_ownership_command(self.get_os_type(), path);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        
        let result = self.run(request, &cmd, CheckRc::Unchecked)?;
        let (rc, out) = cmd_info(&result);

        match rc {
            0 => {},
            _ => { return Ok(None); },
        }

        let mut split = out.split_whitespace();
        let owner = match split.nth(2) {
            Some(x) => x,
            None => { 
                return Err(self.response.is_failed(request, &format!("unexpected output format from {}: {}", cmd, out)));
            }
        };
        // this is a progressive iterator, hence 0 and not 3 for nth() below!
        let group = match split.nth(0) {
            Some(x) => x,
            None => { 
                return Err(self.response.is_failed(request, &format!("unexpected output format from {}: {}", cmd, out))); 
            }
        };
        return Ok(Some((owner.to_string(),group.to_string())));
    }

    pub fn set_owner(&self, request: &Arc<TaskRequest>, remote_path: &String, owner: &String, recurse: Recurse) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::set_owner_command(self.get_os_type(), remote_path, owner, recurse);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        return self.run(request,&cmd,CheckRc::Checked);
    }

    pub fn set_group(&self, request: &Arc<TaskRequest>, remote_path: &String, group: &String, recurse: Recurse) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::set_group_command(self.get_os_type(), remote_path, group, recurse);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        return self.run(request,&cmd,CheckRc::Checked);
    }

    pub fn set_mode(&self, request: &Arc<TaskRequest>, remote_path: &String, mode: &String, recurse: Recurse) -> Result<Arc<TaskResponse>,Arc<TaskResponse>> {
        let get_cmd_result = crate::tasks::cmd_library::set_mode_command(self.get_os_type(), remote_path, mode, recurse);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;
        return self.run(request,&cmd,CheckRc::Checked);
    }

    pub fn get_sha512(&self, request: &Arc<TaskRequest>, path: &String) -> Result<String,Arc<TaskResponse>> {
        // FIXME: move function code here and eliminate
        return self.internal_sha512(request, path);
    }
   

    fn internal_sha512(&self, request: &Arc<TaskRequest>, path: &String) -> Result<String,Arc<TaskResponse>> {
        
        // these games around local command execution should only happen here and not complicate other functions, 
        // local_action/delegate_to will happen at a higher level in the taskfsm.

        let os_type = self.get_os_type();
        let get_cmd_result = crate::tasks::cmd_library::get_sha512_command(os_type, path);
        let cmd = self.unwrap_string_result(&request, &get_cmd_result)?;

        let result = self.run(request, &cmd, CheckRc::Unchecked)?;

        let (rc, out) = cmd_info(&result);
        match rc {
            // we can all unwrap because all possible string lists will have at least 1 element
            0 => {
                let value = out.split_whitespace().nth(0).unwrap().to_string();
                return Ok(value);
            },
            127 => {
                // file not found
                return Ok(String::from(""))
            },
            _ => {
                return Err(self.response.is_failed(request, &format!("checksum failed: {}. {}", path, out)));
            }
        };
    }


    pub fn query_common_file_attributes(&self, request: &Arc<TaskRequest>, remote_path: &String, 
        attributes_in: &Option<FileAttributesEvaluated>, changes: &mut Vec<Field>, recurse: Recurse) -> Result<Option<String>,Arc<TaskResponse>> {

        let remote_mode = self.get_mode(request, remote_path)?;
        
        if remote_mode.is_none() {
            changes.push(Field::Content);
            return Ok(None);
        }

        if recurse == Recurse::Yes {
            changes.push(Field::Owner);
            changes.push(Field::Group);
            changes.push(Field::Mode);
            return Ok(remote_mode);
        }

        if attributes_in.is_some() {
            let attributes = attributes_in.as_ref().unwrap();
            let owner_result = self.get_ownership(request, remote_path)?;
            if owner_result.is_none() {
                return Err(self.response.is_failed(request, &String::from("file was deleted unexpectedly mid-operation")));
            }
            let (remote_owner, remote_group) = owner_result.unwrap();

            if attributes.owner.is_some() {
                if ! remote_owner.eq(attributes.owner.as_ref().unwrap()) { 
                    changes.push(Field::Owner); 
                }
            }
            if attributes.group.is_some() {
                if ! remote_group.eq(attributes.group.as_ref().unwrap())  { 
                    changes.push(Field::Group); 
                }
            }
            if attributes.mode.is_some() {
                if ! remote_mode.as_ref().unwrap().eq(attributes.mode.as_ref().unwrap()) { 
                    changes.push(Field::Mode); 
                }
            }
        }
        return Ok(remote_mode);
    }

    pub fn process_common_file_attributes(&self, 
        request: &Arc<TaskRequest>, 
        remote_path: &String, 
        attributes_in: &Option<FileAttributesEvaluated>, 
        changes: &Vec<Field>,
        recurse: Recurse)

            -> Result<(),Arc<TaskResponse>> {

        if attributes_in.is_none() {
            return Ok(());
        }
        let attributes = attributes_in.as_ref().unwrap();

        for change in changes.iter() {
            match change {
                Field::Owner => {
                    assert!(attributes.owner.is_some(), "owner is set");
                    self.set_owner(request, remote_path, &attributes.owner.as_ref().unwrap(), recurse)?;
                },
                Field::Group => {
                    assert!(attributes.group.is_some(), "owner is set");
                    self.set_group(request, remote_path, &attributes.group.as_ref().unwrap(), recurse)?;
                },
                Field::Mode => {
                    assert!(attributes.mode.is_some(), "owner is set");
                    self.set_mode(request, remote_path, &attributes.mode.as_ref().unwrap(), recurse)?;
                },
                _ => {}
            }
        }
        return Ok(());
    }

    pub fn process_all_common_file_attributes(&self, 
        request: &Arc<TaskRequest>, 
        remote_path: &String, 
        attributes_in: &Option<FileAttributesEvaluated>,
        recurse: Recurse) 
             -> Result<(),Arc<TaskResponse>> {

        let all = Field::all_file_attributes();
        return self.process_common_file_attributes(request, remote_path, attributes_in, &all, recurse);
    }


}