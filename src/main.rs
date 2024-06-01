mod clip;
mod clip_image_processor;
mod config;
mod constants;
mod conversation;
mod llama;
mod model;
mod utils;
use candle_transformers::generation::{LogitsProcessor, Sampling};
use config::{HFGenerationConfig, HFLLaVAConfig, HFPreProcessorConfig};
use constants::*;
use utils::{process_image, tokenizer_image_token};

use crate::llama::Cache;
use crate::{
    config::LLaVAConfig, conversation::Conversation, model::LLaVA, utils::get_model_name_from_path,
};
use anyhow::{bail, Error as E, Result};
use candle_core::{DType, IndexOp, Tensor};
use candle_nn::VarBuilder;
use clap::Parser;
use clip_image_processor::CLIPImageProcessor;
use hf_hub::api::sync::Api;
use std::io::Write;
use std::process::Command;
use tokenizers::Tokenizer;

#[derive(Parser, Debug)]
#[command(author, version, about,long_about=None)]
struct Args {
    #[arg(long, default_value = "liuhaotian/llava-v1.6-vicuna-7b")]
    model_path: String,
    #[arg(long)]
    model_base: Option<String>,
    #[arg(long, default_value = "images/llava_logo.png")]
    image_file: String, // Required
    #[arg(long)]
    conv_mode: Option<String>,
    #[arg(long, default_value_t = 0.2)]
    temperature: f32,
    #[arg(long, default_value_t = 512)]
    max_new_tokens: usize,
    #[arg(long, action)]
    load_8bit: bool, // now useless
    #[arg(long, action)]
    load_4bit: bool, //now useless
    #[arg(long, action)]
    debug: bool, // now useless
    #[arg(long, action)]
    cpu: bool,
    #[arg(long, action)]
    no_kv_cache: bool,
    #[arg(long, default_value = "Is this a cat?")]
    prompt: String,
    /// The seed to use when generating random samples. Copy from candle llama. Not exist in python llava.
    #[arg(long, default_value_t = 299792458)]
    seed: u64,
}

//from https://github.com/huggingface/candle/blob/main/candle-examples/examples/clip/main.rs
fn load_image<T: AsRef<std::path::Path>>(
    path: T,
    processor: &CLIPImageProcessor,
    llava_config: &LLaVAConfig,
    dtype: DType,
) -> anyhow::Result<((u32, u32), Tensor)> {
    let img = image::io::Reader::open(path)?.decode()?;
    let img_tensor = process_image(&img, processor, llava_config)?;
    Ok(((img.width(), img.height()), img_tensor.to_dtype(dtype)?))
}

fn get_model_name(path: &str) -> String {
    path.split('/').last().unwrap().to_string()
}

fn main() -> Result<()> {
    let mut args = Args::parse();
    let device = candle_examples::device(args.cpu)?;
    let api = Api::new()?;
    let api = api.model(args.model_path.clone());
    let model_name = get_model_name(&args.model_path);

    let (llava_config, tokenizer, clip_vision_config, image_processor) = if model_name
        .contains("hf")
    {
        let config_filename = api.get("config.json")?;
        let hf_llava_config: HFLLaVAConfig =
            serde_json::from_slice(&std::fs::read(config_filename)?)?;
        let generation_config_filename = api.get("generation_config.json")?;
        let generation_config: HFGenerationConfig =
            serde_json::from_slice(&std::fs::read(generation_config_filename)?)?;
        let preprocessor_config_filename = api.get("preprocessor_config.json")?;
        let preprocessor_config: HFPreProcessorConfig =
            serde_json::from_slice(&std::fs::read(preprocessor_config_filename)?)?;
        let llava_config =
            hf_llava_config.to_llava_config(&model_name, &generation_config, &preprocessor_config);
        let tokenizer_filename = api.get("tokenizer.json")?;
        let tokenizer = Tokenizer::from_file(tokenizer_filename).map_err(E::msg)?;
        let clip_vision_config = hf_llava_config.to_clip_vision_config();
        (
            llava_config,
            tokenizer,
            Some(clip_vision_config),
            preprocessor_config.to_clip_image_processor(),
        )
    } else {
        let config_filename = api.get("config.json")?;
        let llava_config: LLaVAConfig = serde_json::from_slice(&std::fs::read(config_filename)?)?;
        println!(
            "use python to generate tokenizer.json. Will save tokenizer to tokenizer/tokenizer.json"
        );
        let cmd = format!("python -c \"from transformers import AutoTokenizer;tokenizer=AutoTokenizer.from_pretrained('{}');tokenizer.save_pretrained('tokenizer')\"", args.model_path);
        let output = Command::new("python")
            .args(["-c", &cmd])
            .output()
            .expect("python error!");
        println!("python output: {:?}", output);
        println!("loading tokenizer from tokenizer/tokenizer.json");
        let tokenizer = Tokenizer::from_file("tokenizer/tokenizer.json").map_err(E::msg)?;
        (
            llava_config.clone(),
            tokenizer,
            None,
            CLIPImageProcessor::from_pretrained(&llava_config.mm_vision_tower.unwrap())?,
        )
    };

    let llama_config = llava_config.to_llama_config();
    let dtype: DType = match llava_config.torch_dtype.as_str() {
        "float16" => DType::F16,
        "bfloat16" => DType::BF16,
        _ => bail!("unsupported dtype"),
    };

    let eos_token_id = llava_config.eos_token_id;

    println!("setting kv cache");
    let mut cache = Cache::new(!args.no_kv_cache, dtype, &llama_config, &device)?;

    println!("loading model weights");

    let weight_filenames =
        candle_examples::hub_load_safetensors(&api, "model.safetensors.index.json")?;
    let vb = unsafe { VarBuilder::from_mmaped_safetensors(&weight_filenames, dtype, &device)? };
    let llava: LLaVA = LLaVA::load(vb, &llava_config, clip_vision_config)?;

    println!("generating conv template");
    let image_token_se = format!(
        "{}{}{}",
        DEFAULT_IM_START_TOKEN, DEFAULT_IMAGE_TOKEN, DEFAULT_IM_END_TOKEN
    );
    let qs = if args.prompt.contains(IMAGE_PLACEHOLDER) {
        if llava_config.mm_use_im_start_end {
            args.prompt.replace(IMAGE_PLACEHOLDER, &image_token_se)
        } else {
            args.prompt.replace(IMAGE_PLACEHOLDER, DEFAULT_IMAGE_TOKEN)
        }
    } else if llava_config.mm_use_im_start_end {
        format!("{}\n{}", image_token_se, args.prompt)
    } else {
        format!("{}\n{}", DEFAULT_IMAGE_TOKEN, args.prompt)
    };

    let model_name = get_model_name_from_path(&args.model_path).to_lowercase();
    let conv_mode = if model_name.contains("llama-2") {
        "llava_llama_2"
    } else if model_name.contains("mistral") {
        "mistral_instruct"
    } else if model_name.contains("v1.6-34b") {
        "chatml_direct"
    } else if model_name.contains("v1") {
        "llava_v1"
    } else if model_name.contains("mpt") {
        "mpt"
    } else {
        "llava_v0"
    };
    if args.conv_mode.is_some() && args.conv_mode.as_deref() != Some(conv_mode) {
        println!(
            "Warning: the model is trained with {}, but you are using {}",
            conv_mode,
            args.conv_mode.as_deref().unwrap()
        );
    } else {
        args.conv_mode = Some(conv_mode.to_string());
    }

    let mut conv = match args.conv_mode {
        Some(conv_mode) => match conv_mode.as_str() {
            "chatml_direct" => Conversation::conv_chatml_direct(),
            "llava_v1" => Conversation::conv_llava_v1(),
            _ => todo!("not implement yet"),
        },
        None => bail!("conv_mode is required"),
    };
    conv.append_user_message(Some(&qs));
    conv.append_assistant_message(None);
    let prompt = conv.get_prompt();
    println!("loading image");
    let (image_size, image_tensor) =
        load_image(&args.image_file, &image_processor, &llava_config, dtype)?;
    let image_tensor = image_tensor.to_device(&device)?;

    let mut logits_processor = {
        let temperature = f64::from(args.temperature);
        let sampling = if temperature <= 0. {
            Sampling::ArgMax
        } else {
            Sampling::All { temperature }
        };
        LogitsProcessor::from_sampling(args.seed, sampling)
    };

    // get input tokens
    let tokens = tokenizer_image_token(
        &prompt,
        &tokenizer,
        llava_config.image_token_index as i64,
        &llava_config,
    )?;
    let input_embeds =
        llava.prepare_inputs_labels_for_multimodal(&tokens, &[image_tensor], &[image_size])?;
    //inference loop, based on https://github.com/huggingface/candle/blob/main/candle-examples/examples/llama/main.rs
    let mut tokenizer = candle_examples::token_output_stream::TokenOutputStream::new(tokenizer);
    let mut index_pos = 0;
    let mut _input_embeds = input_embeds.clone();
    for index in 0..args.max_new_tokens {
        let (_, input_embeds_len, _) = _input_embeds.dims3()?;
        let (context_size, context_index) = if cache.use_kv_cache && index > 0 {
            (1, index_pos)
        } else {
            (input_embeds_len, 0)
        };
        let input = _input_embeds.i((.., input_embeds_len.saturating_sub(context_size).., ..))?;
        let logits = llava.forward(&input, context_index, &mut cache)?; //[1,32000]
        let logits = logits.squeeze(0)?;
        let (_, input_len, _) = input.dims3()?;
        index_pos += input_len;
        let next_token = logits_processor.sample(&logits)?;
        let next_token_tensor = Tensor::from_vec(vec![next_token], 1, &device)?;
        let next_embeds = llava.llama.embed(&next_token_tensor)?.unsqueeze(0)?;
        _input_embeds = Tensor::cat(&[_input_embeds, next_embeds], 1)?;
        if next_token == eos_token_id as u32 {
            break;
        }
        if let Some(t) = tokenizer.next_token(next_token)? {
            print!("{t}");
            std::io::stdout().flush()?;
        }
    }
    if let Some(rest) = tokenizer.decode_rest().map_err(E::msg)? {
        print!("{rest}");
    }

    Ok(())
}
