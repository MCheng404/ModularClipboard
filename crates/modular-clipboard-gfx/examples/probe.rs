//! 验证 ash 能否真正加载 Vulkan 运行时并枚举设备。
//!
//! 这是一个独立探针：不依赖 UI 窗口，仅用 ash 加载库、创建实例、
//! 枚举物理设备。用于在接入完整渲染管线前先确认底层链路可用。

fn main() {
    let entry = match unsafe { ash::Entry::load() } {
        Ok(e) => {
            println!("OK ash::Entry::load()");
            e
        }
        Err(e) => {
            println!("FAIL Entry::load: {e}");
            std::process::exit(1);
        }
    };
    println!("OK entry loaded");

    let exts: Vec<*const i8> = vec![c"VK_KHR_surface".as_ptr()];
    let app = ash::vk::ApplicationInfo::default()
        .application_name(c"probe")
        .api_version(ash::vk::API_VERSION_1_1);
    let ci = ash::vk::InstanceCreateInfo::default()
        .application_info(&app)
        .enabled_extension_names(&exts);

    let instance = match unsafe { entry.create_instance(&ci, None) } {
        Ok(i) => i,
        Err(e) => {
            println!("FAIL create_instance: {e}");
            std::process::exit(2);
        }
    };
    println!("OK create_instance");

    let devices = match unsafe { instance.enumerate_physical_devices() } {
        Ok(d) => d,
        Err(e) => {
            println!("FAIL enumerate: {e}");
            std::process::exit(3);
        }
    };
    println!("OK enumerate_physical_devices -> {} devices", devices.len());

    for d in &devices {
        let p = unsafe { instance.get_physical_device_properties(*d) };
        let name: Vec<u8> = p.device_name.iter().map(|c| *c as u8).take_while(|b| *b != 0).collect();
        let kind = match p.device_type {
            ash::vk::PhysicalDeviceType::DISCRETE_GPU => "独显",
            ash::vk::PhysicalDeviceType::INTEGRATED_GPU => "核显",
            ash::vk::PhysicalDeviceType::VIRTUAL_GPU => "虚拟",
            ash::vk::PhysicalDeviceType::CPU => "CPU",
            _ => "其它",
        };
        println!("  - {} [{}] api {}.{}", String::from_utf8_lossy(&name), kind,
            ash::vk::api_version_major(p.api_version), ash::vk::api_version_minor(p.api_version));
    }

    let fams = unsafe { instance.get_physical_device_queue_family_properties(devices[0]) };
    println!("OK queue families: {}", fams.len());

    unsafe { instance.destroy_instance(None) };
    println!("ALL OK");
}
