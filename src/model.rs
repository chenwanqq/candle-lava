use crate::clip::clip_vit_large_patch14_336;
use crate::llama::Cache;
use crate::llama::Llama;
use crate::utils::get_anyres_image_grid_shape;
use crate::IMAGE_TOKEN_INDEX;
use candle_core::bail;
use candle_core::Device;
use candle_core::IndexOp;
use candle_core::Result;
use candle_core::Tensor;
use candle_nn::Module;
use candle_nn::{seq, Activation, Sequential, VarBuilder};
use candle_transformers::models::clip::vision_model::ClipVisionConfig;
use candle_transformers::models::with_tracing::linear;
use regex::Regex;

use crate::clip::ClipVisionTransformerWithHiddenStates;
use crate::config::LLaVAConfig;

fn mlp_gelu_match(mm_projector_type: &str) -> Option<usize> {
    let mlp_gelu_regex = Regex::new(r"^mlp(\d+)x_gelu$").unwrap();

    if let Some(captures) = mlp_gelu_regex.captures(mm_projector_type) {
        if let Some(match_str) = captures.get(1) {
            let match_str = match_str.as_str();
            match_str.parse::<usize>().ok()
        } else {
            None
        }
    } else {
        None
    }
}

fn unpad_image(tensor: &Tensor, original_size: &(u32, u32)) -> Result<Tensor> {
    assert_eq!(tensor.dims().len(), 3);
    let (original_width, original_height) = *original_size;
    let tensor_dims = tensor.dims();
    let current_height = tensor_dims[1];
    let current_width = tensor_dims[2];
    let original_aspect_ratio = (original_width as f32) / (original_height as f32);
    let current_aspect_ratio = (current_width as f32) / (current_height as f32);
    if original_aspect_ratio > current_aspect_ratio {
        let scale_factor = (current_width as f32) / (original_width as f32);
        let new_height = (original_height as f32 * scale_factor).floor() as usize;
        let padding = (current_height - new_height) / 2;
        tensor.i((.., padding..current_width - padding, ..))
    } else {
        let scale_factor = (current_height as f32) / (original_height as f32);
        let new_width = (original_width as f32 * scale_factor).floor() as usize;
        let padding = (current_width - new_width) / 2;
        tensor.i((.., .., padding..current_width - padding))
    }
}

pub struct IdentityMap {}

impl Module for IdentityMap {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        Ok(x.clone())
    }
}

pub struct MMProjector {
    pub modules: Sequential,
}

impl MMProjector {
    pub fn load(vb: &VarBuilder, config: &LLaVAConfig) -> Result<Self> {
        if config.mm_projector_type == "linear" {
            let linear = linear(
                config.mm_hidden_size,
                config.hidden_size,
                vb.pp("model.mm_projector.0"),
            )?;
            let modules = seq().add(linear);
            Ok(Self { modules })
        } else if let Some(mlp_depth) = mlp_gelu_match(&config.mm_projector_type) {
            let mut modules = seq().add(linear(
                config.mm_hidden_size,
                config.hidden_size,
                vb.pp("model.mm_projector.0"),
            )?);
            for i in 1..mlp_depth {
                modules = modules.add(Activation::Gelu).add(linear(
                    config.hidden_size,
                    config.hidden_size,
                    vb.pp(format!("model.mm_projector.{}", i * 2)),
                )?);
            }
            Ok(Self { modules })
        } else if config.mm_projector_type == "identity" {
            Ok(Self {
                modules: seq().add(IdentityMap {}),
            })
        } else {
            bail!(
                "Unsupported MM projector type: {}",
                config.mm_projector_type
            )
        }
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        self.modules.forward(x)
    }
}

pub struct ClipVisionTower {
    model: ClipVisionTransformerWithHiddenStates,
    select_layer: isize,
    select_feature_method: String,
    pub config: ClipVisionConfig,
}

impl ClipVisionTower {
    pub fn load(vb: &VarBuilder, config: &LLaVAConfig) -> Result<Self> {
        let clip_vision_config = if config.mm_vision_tower == "openai/clip-vit-large-patch14-336" {
            clip_vit_large_patch14_336()
        } else {
            bail!(
                "vision tower {} is not implemented yet",
                config.mm_vision_tower
            )
        };
        // to simulate hidden_state of python version clip
        let select_layer = match config.mm_vision_select_layer {
            -1 | -2 => config.mm_vision_select_layer,
            _ => bail!(
                "Unsupported select layer: {}",
                config.mm_vision_select_layer
            ),
        };
        let model = ClipVisionTransformerWithHiddenStates::new(
            vb.pp("model.vision_tower.vision_tower.vision_model"),
            &clip_vision_config,
        )?;
        Ok(Self {
            model,
            select_layer,
            select_feature_method: config.mm_vision_select_feature.clone(),
            config: clip_vision_config,
        })
    }

    pub fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let result = self.model.output_hidden_states(x)?;
        let index = result.len() as isize + self.select_layer;
        let result = result[index as usize].clone();
        if self.select_feature_method == "cls_patch" {
            Ok(result)
        } else {
            result.i((.., 1..))
        }
    }

    pub fn num_patches_per_side(&self) -> usize {
        self.config.image_size / self.config.patch_size
    }
}

pub struct LLaVA {
    pub clip_vision_tower: ClipVisionTower,
    pub image_newline: Tensor,
    pub mm_projector: MMProjector,
    pub llama: Llama,
    config: LLaVAConfig,
    device: Device,
}

impl LLaVA {
    pub fn load(vb: VarBuilder, config: &LLaVAConfig) -> Result<Self> {
        let device = vb.device().clone();
        let clip_vision_tower = ClipVisionTower::load(&vb, config)?;
        let mm_projector = MMProjector::load(&vb, config)?;
        let llama_config = config.to_llama_config();
        let image_newline = vb
            .get(&[config.hidden_size], "model.image_newline")?
            .to_device(&device)?;
        let llama = Llama::load(vb, &llama_config)?;
        Ok(Self {
            clip_vision_tower,
            image_newline,
            mm_projector,
            llama,
            config: (*config).clone(),
            device,
        })
    }

    pub fn encode_images(&self, x: &Tensor) -> Result<Tensor> {
        let image_features = self.clip_vision_tower.forward(x)?;
        let image_features = self.mm_projector.forward(&image_features)?;
        Ok(image_features)
    }
    // currently only for single image, 4 dim tensor
    pub fn prepare_inputs_labels_for_multimodal(
        &self,
        input_ids: &Tensor,
        images: &Vec<Tensor>,
        image_sizes: &Vec<(u32, u32)>,
    ) -> Result<Tensor> {
        //TODO: process of multiple images/ new line
        // 576: 336(input size)/14(patch size)=24 24*24+1(class)=577 577-1=576
        let concat_images = Tensor::cat(images, 0)?;
        let image_features_together = self.encode_images(&concat_images)?;
        let split_sizes = images
            .iter()
            .map(|x| x.shape().dims()[0])
            .collect::<Vec<usize>>();
        // can be replaced by split
        let mut index_pos = 0;
        let mut image_features = Vec::new();
        for split_size in split_sizes.iter() {
            image_features.push(image_features_together.i(index_pos..index_pos + (*split_size))?);
        }
        println!(
            "image_features: {:?} {:?}",
            image_features.len(),
            image_features[0].shape()
        );
        let mm_patch_merge_type = &self.config.mm_patch_merge_type;
        let image_aspect_ratio = &self.config.image_aspect_ratio;
        let image_features = if mm_patch_merge_type == "flat" {
            image_features
                .iter()
                .map(|x| x.flatten(0, 1).unwrap())
                .collect::<Vec<Tensor>>()
        } else if mm_patch_merge_type.starts_with("spatial") {
            let mut new_image_features = Vec::new();
            for (image_idx, image_feature) in image_features.iter().enumerate() {
                let new_image_feature = if image_feature.dims()[0] > 1 {
                    let base_image_feature = image_feature.get(0).unwrap();
                    let patch_image_feature = image_feature.i(1..).unwrap();
                    let height = self.clip_vision_tower.num_patches_per_side();
                    let width = height;
                    assert_eq!(height * width, base_image_feature.dims()[0]);
                    let image_size = image_sizes[image_idx];
                    let new_image_feature = if image_aspect_ratio == "anyres" {
                        let (num_patch_width, num_patch_height) = get_anyres_image_grid_shape(
                            image_size,
                            &self.config.image_grid_pinpoints,
                            self.clip_vision_tower.config.image_size as u32,
                        );
                        println!("num_patch_width: {}, num_patch_height: {}", num_patch_width, num_patch_height);
                        patch_image_feature.reshape((
                            num_patch_height as usize,
                            num_patch_width as usize,
                            height,
                            width,
                            (),
                        ))?
                    } else {
                        todo!("not implemented in original python LLaVA yet")
                    };
                    let new_image_feature = if mm_patch_merge_type.contains("unpad") {
                        println!("before transform new_image_feature.shape {:?}",new_image_feature.shape());
                        let new_image_feature = new_image_feature
                            .permute((4, 0, 2, 1, 3))?
                            .flatten(1, 2)?
                            .flatten(2, 3)?;
                        println!("before unpad new_image_feature.shape: {:?}",new_image_feature.shape());
                        let new_image_feature = unpad_image(&new_image_feature, &image_size)?;
                        println!("before cat new_image_feature.shape: {:?}",new_image_feature.shape());
                        let new_image_feature_dims = new_image_feature.dims();
                        let image_new_line = self
                            .image_newline
                            .reshape((self.config.hidden_size, 1, 1))?
                            .broadcast_as((new_image_feature_dims[0], new_image_feature_dims[1], 1))?;
                        let new_image_feature =
                            Tensor::cat(&[new_image_feature, image_new_line], 2)?;
                        new_image_feature.flatten(1, 2)?.transpose(0, 1)?
                    } else {
                        new_image_feature.permute((0, 2, 1, 3, 4))?.flatten(0, 3)?
                    };
                    Tensor::cat(&[base_image_feature,new_image_feature],0)?
                } else {
                    let new_image_feature = image_feature.get(0).unwrap();
                    let new_image_feature = if mm_patch_merge_type.contains("unpad") {
                        Tensor::cat(
                            &[
                                new_image_feature,
                                self.image_newline.clone().unsqueeze(0).unwrap(),
                            ],
                            0,
                        )
                        .unwrap()
                    } else {
                        new_image_feature
                    };
                    new_image_feature
                };
                new_image_features.push(new_image_feature);
            }
            new_image_features
        } else {
            bail!("Unexpected mm_patch_merge_type: {mm_patch_merge_type}")
        };
        println!(
            "image_features: {:?} {:?}",
            image_features.len(),
            image_features[0].shape()
        );
        todo!()
        /*
        let (batch_size, input_len) = input_ids.shape().dims2()?;
        //TODO: attention mask
        println!("image_features: {:?}", image_features.shape());
        println!("input_ids: {:?}", input_ids.shape());
        // can easily be replaced by nonzero if it is implemented in candle
        let input_ids_vec = input_ids.squeeze(0)?.to_vec1::<i64>()?;
        let mut image_indices = {
            let mut image_indices = vec![-1 as i64];
            image_indices.extend(
                input_ids_vec
                    .iter()
                    .enumerate()
                    .filter_map(|(i, x)| {
                        if *x == IMAGE_TOKEN_INDEX as i64 {
                            Some(i as i64)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<i64>>(),
            );
            image_indices
        };
        let input_ids_noim = input_ids_vec
            .iter()
            .filter_map(|x| {
                if *x != IMAGE_TOKEN_INDEX as i64 {
                    Some(*x)
                } else {
                    None
                }
            })
            .collect::<Vec<i64>>();
        let input_ids_noim_len = input_ids_noim.len();
        image_indices.push(input_ids_noim_len as i64);
        let input_ids_noim = Tensor::from_vec(input_ids_noim, input_ids_noim_len, &self.device)?;
        println!("input_ids_noim: {:?}", input_ids_noim.shape());
        let cur_input_embeds = self.llama.embed(&input_ids_noim)?;
        println!("cur_input_embeds: {:?}", cur_input_embeds.shape());
        println!("image_indices: {:?}", image_indices);
        // can be replace by split if it is implemented in candle
        let input_embed_no_ims = {
            let mut input_embeds = Vec::new();
            for i in 0..image_indices.len() - 1 {
                let start = (image_indices[i] + 1) as usize;
                let end = image_indices[i + 1] as usize;
                println!("start: {}, end: {}", start, end);
                input_embeds.push(cur_input_embeds.i((start..end, ..))?)
            }
            input_embeds
        };
        println!(
            "input_embed_no_ims: {:?} {:?}",
            input_embed_no_ims.len(),
            input_embed_no_ims[0].shape()
        );

        let mut cur_new_input_embeds = Vec::new();
        //concat of text and images and text TODO: multiple images
        cur_new_input_embeds.push(input_embed_no_ims[0].clone());
        cur_new_input_embeds.push(image_features);
        cur_new_input_embeds.push(input_embed_no_ims[1].clone());
        let new_input_embeds = Tensor::cat(&cur_new_input_embeds, 0)?;
        //trancate
        let new_input_embeds =
            if let Some(tokenizer_model_max_length) = self.config.tokenizer_model_max_length {
                let (new_input_embeds_length,_) = new_input_embeds.shape().dims2()?;
                if new_input_embeds_length > tokenizer_model_max_length {
                    new_input_embeds.i((..tokenizer_model_max_length, ..))?
                } else {
                    new_input_embeds
                }
            } else {
                new_input_embeds
            };
        println!("new_input_embeds: {:?}", new_input_embeds.shape());
        //TODO: padding multiple tokens
        Ok(new_input_embeds.unsqueeze(0)?)
        */
    }

    pub fn generate(
        &self,
        input_embeds: &Tensor,
        position_id: usize,
        cache: &mut Cache,
    ) -> Result<Tensor> {
        self.llama.generate(&input_embeds, position_id, cache)
    }
}
