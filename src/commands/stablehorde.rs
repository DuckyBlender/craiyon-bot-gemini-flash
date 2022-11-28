use std::fmt::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use counter::Counter;
use image::ImageFormat;
use tdlib::enums::{
    FormattedText, InlineKeyboardButtonType, InputFile, InputMessageContent, ReplyMarkup,
    TextParseMode,
};
use tdlib::functions;
use tdlib::types::{
    InlineKeyboardButton, InlineKeyboardButtonTypeUrl, InputFileLocal, InputMessagePhoto, Message,
    ReplyMarkupInlineKeyboard, TextParseModeMarkdown,
};
use tempfile::NamedTempFile;

use super::CommandError::MissingArgument;
use super::{CommandResult, CommandTrait};
use crate::apis::stablehorde::{self, Status};
use crate::command_context::CommandContext;
use crate::ratelimit::RateLimiter;
use crate::utils::{
    check_prompt, escape_markdown, format_duration, image_collage, TruncateWithEllipsis,
};

pub struct StableHorde {
    command_names: &'static [&'static str],
    command_description: &'static str,
    model: &'static str,
    size: u32,
}

impl StableHorde {
    pub fn stable_diffusion_2() -> Self {
        Self {
            command_names: &["stable_diffusion_2", "sd2"],
            command_description: "generate images using Stable Diffusion 2.0",
            model: "stable_diffusion_2.0",
            size: 768,
        }
    }

    pub fn stable_diffusion() -> Self {
        Self {
            command_names: &["stable_diffusion", "sd"],
            command_description: "generate images using Stable Diffusion",
            model: "stable_diffusion",
            size: 512,
        }
    }

    pub fn waifu_diffusion() -> Self {
        Self {
            command_names: &["waifu_diffusion", "wd"],
            command_description: "generate images using Waifu Diffusion",
            model: "waifu_diffusion",
            size: 512,
        }
    }

    pub fn furry_diffusion() -> Self {
        Self {
            command_names: &["furry_diffusion", "fd"],
            command_description: "generate images using Furry Epoch",
            model: "Furry Epoch",
            size: 512,
        }
    }
}

#[async_trait]
impl CommandTrait for StableHorde {
    fn command_names(&self) -> &[&str] {
        self.command_names
    }

    fn description(&self) -> Option<&'static str> {
        Some(self.command_description)
    }

    fn rate_limit(&self) -> RateLimiter<i64> {
        RateLimiter::new(3, 300)
    }

    #[allow(clippy::too_many_lines)]
    async fn execute(&self, ctx: Arc<CommandContext>, arguments: Option<String>) -> CommandResult {
        let prompt = arguments.ok_or(MissingArgument("prompt to generate"))?;

        if let Some(issue) = check_prompt(&prompt) {
            log::info!("prompt rejected: {issue:?}");
            Err(issue)?;
        }

        let request_id =
            stablehorde::generate(ctx.http_client.clone(), self.model, &prompt, self.size)
                .await??;

        let mut status_msg: Option<Message> = None;
        let escaped_prompt = escape_markdown(prompt);
        let start = Instant::now();
        let mut show_volunteer_notice = false;
        let mut last_edit: Option<Instant> = None;
        let mut last_status = None;
        loop {
            let status = stablehorde::check(ctx.http_client.clone(), &request_id).await??;

            if status.done {
                break;
            };

            if status.wait_time >= 30 {
                show_volunteer_notice = true;
            }

            if last_status.as_ref() != Some(&status) {
                // the message doesn't exist yet or was edited more than 12 seconds ago
                if last_edit
                    .map_or(true, |last_edit| last_edit.elapsed() >= Duration::from_secs(12))
                {
                    let text = format_status(&status, &escaped_prompt, show_volunteer_notice);
                    status_msg = Some(match status_msg {
                        None => {
                            ctx.message_queue
                                .wait_for_message(ctx.reply_markdown(text).await?.id)
                                .await?
                        }
                        Some(status_msg) => ctx.edit_message_markdown(status_msg.id, text).await?,
                    });

                    last_edit = Some(Instant::now());
                    last_status = Some(status);
                }
            };

            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        let duration = start.elapsed();
        let results = stablehorde::results(ctx.http_client.clone(), &request_id).await??;
        let mut workers = Counter::<String>::new();
        let images = results
            .into_iter()
            .flat_map(|generation| {
                workers[&generation.worker_name] += 1;
                base64::decode(generation.img)
            })
            .flat_map(|image| image::load_from_memory_with_format(&image, ImageFormat::WebP))
            .collect::<Vec<_>>();

        let image = image_collage(images, (self.size, self.size), 2, 8);
        let mut temp_file = NamedTempFile::new().unwrap();
        image.write_to(temp_file.as_file_mut(), ImageFormat::Png).unwrap();

        let FormattedText::FormattedText(formatted_text) = functions::parse_text_entities(
            format!(
                "generated *{}* in {} by {}\\.",
                escaped_prompt,
                format_duration(duration.as_secs()),
                workers
                    .most_common()
                    .into_iter()
                    .map(|(mut k, v)| {
                        k.truncate_with_ellipsis(64);
                        if v > 1 {
                            write!(k, " ({v})").unwrap();
                        }
                        escape_markdown(k)
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            TextParseMode::Markdown(TextParseModeMarkdown { version: 2 }),
            ctx.client_id,
        )
        .await
        .unwrap();

        let message = ctx
            .reply_custom(
                InputMessageContent::InputMessagePhoto(InputMessagePhoto {
                    photo: InputFile::Local(InputFileLocal {
                        path: temp_file.path().to_str().unwrap().into(),
                    }),
                    thumbnail: None,
                    added_sticker_file_ids: Vec::new(),
                    width: image.width().try_into().unwrap(),
                    height: image.height().try_into().unwrap(),
                    caption: Some(formatted_text),
                    ttl: 0,
                }),
                Some(ReplyMarkup::InlineKeyboard(ReplyMarkupInlineKeyboard {
                    rows: vec![vec![InlineKeyboardButton {
                        text: "generated thanks to Stable Horde".into(),
                        r#type: InlineKeyboardButtonType::Url(InlineKeyboardButtonTypeUrl {
                            url: "https://stablehorde.net/".into(),
                        }),
                    }]],
                })),
            )
            .await?;

        ctx.message_queue.wait_for_message(message.id).await?;

        if let Some(status_msg) = status_msg {
            ctx.delete_message(status_msg.id).await.ok();
        }

        Ok(())
    }
}

fn format_status(status: &Status, escaped_prompt: &str, volunteer_notice: bool) -> String {
    let queue_info = if status.queue_position > 0 {
        format!("queue position: {}\n", status.queue_position)
    } else {
        String::new()
    };

    let mut text = format!(
        "generating {escaped_prompt}…\n{queue_info}`{}` ETA: {}",
        progress_bar(
            status.waiting.unsigned_abs() as usize,
            status.processing.unsigned_abs() as usize,
            status.finished.unsigned_abs() as usize
        ),
        format_duration(status.wait_time.try_into().unwrap())
    );

    if volunteer_notice {
        text.push_str(concat!(
            "\n\nStable Horde is run by volunteers\\. ",
            "to make waiting times shorter, ",
            "[consider joining yourself](https://stablehorde.net/)\\!"
        ));
    };

    text
}

fn progress_bar(waiting: usize, processing: usize, finished: usize) -> String {
    let mut bar = String::with_capacity(4 * (waiting + processing + finished) + 2);
    bar.push('[');
    bar.push_str(&"=".repeat(5 * finished));
    bar.push_str(&"-".repeat(5 * processing));
    bar.push_str(&" ".repeat(5 * waiting));
    bar.push(']');
    bar
}