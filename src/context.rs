use crate::sys::{self, Interface};
use crate::winrt;
use pollster::FutureExt;
use std::cell::{Cell, RefCell};
use wgpu::util::DeviceExt;

#[repr(C)]
#[derive(Copy, Clone, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct SimpleVertex {
    position: [f32; 3],
    color: [f32; 3],
}

#[allow(
    dead_code,
    reason = "we just have to keep `dxgi_factory` and `target` alive"
)]
pub struct Context {
    device: wgpu::Device,
    queue: wgpu::Queue,

    hwnd: sys::HWND,
    dcomp: sys::IDCompositionDevice,
    target: sys::IDCompositionTarget,
    visual: sys::IDCompositionVisual,
    pub partner: winrt::IDCompositionDesktopDevicePartner6,

    surface: sys::IDCompositionVirtualSurface,
    d3d11_device: sys::ID3D11Device,
    d3d11_context: sys::ID3D11DeviceContext,
    width: Cell<u32>,
    height: Cell<u32>,

    render_pipeline: wgpu::RenderPipeline,
    vertex_buffer: wgpu::Buffer,
    texture: RefCell<Option<wgpu::Texture>>,
}

impl Context {
    pub fn new(hwnd: sys::HWND) -> Self {
        unsafe {
            winrt::init_com_single_threaded();
            winrt::make_window_eligible_for_dcomp_resize_sync(hwnd);
        }

        // Enumerate all adapters, and obtain the Intel adapter if it exists
        // (this will be important later on, as there are specific fixes)
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::DX12,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions::default())
            .block_on()
            .unwrap();

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("One-shot renderer"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::downlevel_defaults(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
                ..Default::default()
            })
            .block_on()
            .unwrap();

        // Assemble the compositor visual tree. This along with the functions from `winrt::`
        // make resizing slippery smooth.
        let d3d12_device = unsafe { device.as_hal::<wgpu_hal::api::Dx12>() }
            .unwrap()
            .raw_device()
            .clone();
        let d3d12_queue = unsafe { queue.as_hal::<wgpu_hal::api::Dx12>() }
            .unwrap()
            .as_raw()
            .clone();
        let mut d3d11_device: Option<sys::ID3D11Device> = None;
        let mut d3d11_context: Option<sys::ID3D11DeviceContext> = None;
        unsafe {
            sys::D3D11On12CreateDevice(
                &d3d12_device,
                sys::D3D11_CREATE_DEVICE_BGRA_SUPPORT.0 as u32,
                None,
                Some(&[Some(d3d12_queue.clone().into())]),
                0,
                Some(&mut d3d11_device),
                Some(&mut d3d11_context),
                None,
            )
        }
        .unwrap();
        let d3d11_device = d3d11_device.unwrap();
        let d3d11_context = d3d11_context.unwrap();

        let dcomp: sys::IDCompositionDevice =
            unsafe { sys::DCompositionCreateDevice3(&d3d11_device) }.unwrap();
        let partner = dcomp
            .cast::<winrt::IDCompositionDesktopDevicePartner6>()
            .unwrap();
        let target: sys::IDCompositionTarget =
            unsafe { dcomp.CreateTargetForHwnd(hwnd, false) }.unwrap();
        let visual: sys::IDCompositionVisual = unsafe { dcomp.CreateVisual() }.unwrap();
        unsafe { target.SetRoot(&visual) }.unwrap();

        let mut rect: sys::RECT = Default::default();
        unsafe { sys::GetWindowRect(hwnd, &mut rect) }.unwrap();
        let width = (rect.right - rect.left) as u32;
        let height = (rect.bottom - rect.top) as u32;

        let surface = unsafe {
            dcomp
                .CreateVirtualSurface(
                    width,
                    height,
                    sys::DXGI_FORMAT_B8G8R8A8_UNORM,
                    sys::DXGI_ALPHA_MODE_PREMULTIPLIED,
                )
                .unwrap()
        };
        unsafe { visual.SetContent(&surface) }.unwrap();

        // For the sake of example, we're going to compile a simple shader with wgpu.
        let vertices = [
            SimpleVertex {
                position: [0.0, 0.5, 0.0],
                color: [1.0, 0.0, 0.0],
            },
            SimpleVertex {
                position: [0.5, -0.5, 0.0],
                color: [0.0, 1.0, 0.0],
            },
            SimpleVertex {
                position: [-0.5, -0.5, 0.0],
                color: [0.0, 0.0, 1.0],
            },
        ];

        let shader_module = device.create_shader_module(wgpu::include_wgsl!("shader.wgsl"));
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("Vertex Buffer"),
            contents: bytemuck::cast_slice(&vertices),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let render_pipeline_layout =
            device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("Render Pipeline Layout"),
                bind_group_layouts: &[],
                immediate_size: 0,
            });
        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("Render Pipeline"),
            layout: Some(&render_pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader_module,
                entry_point: Some("vs_main"),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<SimpleVertex>() as wgpu::BufferAddress,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &[
                        wgpu::VertexAttribute {
                            offset: 0,
                            shader_location: 0,
                            format: wgpu::VertexFormat::Float32x3,
                        },
                        wgpu::VertexAttribute {
                            offset: std::mem::size_of::<[f32; 3]>() as wgpu::BufferAddress,
                            shader_location: 1,
                            format: wgpu::VertexFormat::Float32x3,
                        },
                    ],
                })],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader_module,
                entry_point: Some("ps_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                polygon_mode: wgpu::PolygonMode::Fill,
                unclipped_depth: false,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        Self {
            device,
            queue,
            hwnd,
            dcomp,
            target,
            visual,
            partner,
            d3d11_device,
            d3d11_context,
            surface,
            width: Cell::new(800),
            height: Cell::new(600),
            render_pipeline,
            vertex_buffer,
            texture: RefCell::new(None),
        }
    }

    pub fn resize(&self, width: u32, height: u32) {
        self.width.set(width);
        self.height.set(height);

        let handle = unsafe { winrt::get_resize_dcomp_sync_object(self.hwnd) };

        // If the window has no area, we don't want to draw anything, but we still want
        // to go through the smooth resizing pipeline to prevent stutters.
        if !(width == 0 || height == 0) {
            unsafe { self.surface.Resize(width, height) }.unwrap();
            *self.texture.borrow_mut() =
                Some(self.device.create_texture(&wgpu::TextureDescriptor {
                    label: Some("DComp intermediate"),
                    size: wgpu::Extent3d {
                        width,
                        height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
                    view_formats: &[],
                }));

            self.draw();
        }

        unsafe {
            self.partner.synchronized_commit(handle).unwrap();
            winrt::signal_dcomp_resize_completion();
        }
    }

    pub fn draw(&self) {
        // Render to the `wgpu::Texture` normally, polling at the end to ensure that the
        // contents are fully drawn.
        let texture_borrow = self.texture.borrow();
        let texture = texture_borrow.as_ref().unwrap();
        let texture_view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &texture_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.0,
                            g: 0.2,
                            b: 0.4,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            render_pass.set_viewport(
                0.0,
                0.0,
                self.width.get() as f32,
                self.height.get() as f32,
                0.0,
                1.0,
            );
            render_pass.set_pipeline(&self.render_pipeline);
            render_pass.set_vertex_buffer(0, self.vertex_buffer.slice(..));
            render_pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        self.device
            .poll(wgpu::PollType::wait_indefinitely())
            .unwrap();

        // Obtain the contents as an `ID3D12Resource` which can be lowered to an `ID3D11Resource`
        // then to a source `ID3D11Texture2D`.
        let wgpu_resource_hal = unsafe { texture.as_hal::<wgpu_hal::api::Dx12>() }.unwrap();
        let wgpu_resource = unsafe { wgpu_resource_hal.raw_resource() }.clone();

        let d3d11on12: sys::ID3D11On12Device = self.d3d11_device.cast().unwrap();

        // Begin the `ICompositionVisual::BeginDraw` to get the destination `ID3D11Texture2D`.
        let mut offset = sys::POINT::default();
        let dest_texture: sys::ID3D11Texture2D =
            unsafe { self.surface.BeginDraw(None, &mut offset) }.unwrap();

        let mut src_resource: Option<sys::ID3D11Resource> = None;
        unsafe {
            d3d11on12
                .CreateWrappedResource(
                    &wgpu_resource,
                    &sys::D3D11_RESOURCE_FLAGS {
                        BindFlags: 0,
                        MiscFlags: 0,
                        CPUAccessFlags: 0,
                        StructureByteStride: 0,
                    },
                    sys::D3D12_RESOURCE_STATE_RENDER_TARGET,
                    sys::D3D12_RESOURCE_STATE_RENDER_TARGET,
                    &mut src_resource,
                )
                .unwrap();

            let src_resource = src_resource.unwrap();
            d3d11on12.AcquireWrappedResources(&[Some(src_resource.clone())]);

            // Copy the source (rendered by Wgpu) to the destination (composition surface).
            self.d3d11_context.CopySubresourceRegion(
                &dest_texture,
                0,
                offset.x as u32,
                offset.y as u32,
                0,
                &src_resource,
                0,
                Some(&sys::D3D11_BOX {
                    left: 0,
                    top: 0,
                    front: 0,
                    right: self.width.get(),
                    bottom: self.height.get(),
                    back: 1,
                }),
            );

            d3d11on12.ReleaseWrappedResources(&[Some(src_resource.clone())]);
            self.surface.EndDraw().unwrap();
        }
    }
}
