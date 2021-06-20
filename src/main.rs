use std::env;
use serenity::{
    async_trait,
    model::{gateway::Ready, interactions::{Interaction, InteractionResponseType, ApplicationCommand}},
    prelude::*,
};
use serenity::model::prelude::*;
use image_of_images_creator::*;
use std::sync::Arc;
use image::ColorType;
use std::time::Duration;
use std::io::{Read, Cursor};
use std::sync::atomic::{AtomicU8, Ordering};

struct Handler {
    image_dictionary: Arc<ImageDictionary>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn guild_create(&self, ctx: Context, guild: Guild, _is_new: bool) {
        println!("Registering commands for guild {:?}", guild.name);
        for c in guild.get_application_commands(&ctx.http).await.unwrap() {
            println!("Unregistering command {}", c.name);
            guild.delete_application_command(&ctx.http, c.id).await.unwrap();
        }
        println!("Registering command transform");
        guild.create_application_command(&ctx.http, |a| {
            a
                .name("transform")
                .description("Transform an image/your profile picture")
                .create_option(|o| {
                    o.name("avatar")
                        .kind(ApplicationCommandOptionType::SubCommand)
                        .description("Transform your avatar")
                        .create_sub_option(|o| o
                            .name("target_user")
                            .description("User of whom the avatar will be taken from")
                            .required(false)
                            .kind(ApplicationCommandOptionType::User)
                        )
                })
                .create_option(|o| {
                    o.name("image")
                        .kind(ApplicationCommandOptionType::SubCommand)
                        .description("Transform the given image")
                        .create_sub_option(|o| o
                            .name("image_url")
                            .description("Image to be downloaded")
                            .required(true)
                            .kind(ApplicationCommandOptionType::String)
                        )
                })
        }).await.unwrap();
    }

    async fn ready(&self, ctx: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);

        let interactions = ApplicationCommand::get_global_application_commands(&ctx.http).await.unwrap();

        println!("I have the following global slash command(s): {:?}", interactions);
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let guild_id = match interaction.guild_id {
            Some(id) => id,
            None => return,
        };
        let data = match &interaction.data {
            Some(InteractionData::ApplicationCommand(d)) => d,
            _ => return,
        };
        let member = interaction.member.as_ref().unwrap();
        let command = guild_id.get_application_command(&ctx.http, data.id).await.unwrap();

        if command.name == "transform" {
            let default_avatar_url = member.user.default_avatar_url();

            let image_url = match data.options[0].name.as_str() {
                "avatar" => {
                    data.options[0].options.get(0).map(|o| match o.resolved.as_ref().unwrap() {
                        ApplicationCommandInteractionDataOptionValue::User(u, ..) => u,
                        _ => unreachable!()
                    }).unwrap_or(&member.user)
                        .avatar_url()
                        .unwrap_or(default_avatar_url)
                        .replace("webp", "png")
                },
                "image" => {
                    let url = match data.options[0].options[0].resolved.as_ref().unwrap() {
                        ApplicationCommandInteractionDataOptionValue::String(u) => u.clone(),
                        _ => unreachable!()
                    };
                    match reqwest::Url::parse(&url) {
                        Ok(u) if u.host_str().map(|a| a.ends_with("discordapp.com") | a.ends_with("discordapp.net")).unwrap_or(false) => (),
                        Err(..) => {
                            interaction
                                .create_interaction_response(&ctx.http, move |response| {
                                    response
                                        .kind(InteractionResponseType::ChannelMessageWithSource)
                                        .interaction_response_data(|d| {
                                            d.content("Invalid url")
                                        })
                                })
                                .await
                                .unwrap();
                            return
                        },
                        _ => {
                            interaction
                                .create_interaction_response(&ctx.http, move |response| {
                                    response
                                        .kind(InteractionResponseType::ChannelMessageWithSource)
                                        .interaction_response_data(|d| {
                                            d.content("You can only use discord hosted images")
                                        })
                                })
                                .await
                                .unwrap();
                            return
                        },
                    }
                    url
                },
                _ => unreachable!()
            };

            interaction
                .create_interaction_response(&ctx.http, move |response| {
                    response
                        .kind(InteractionResponseType::DeferredChannelMessageWithSource)
                })
                .await
                .unwrap();

            let response = match reqwest::get(&image_url).await {
                Ok(r) => r,
                Err(_) => {
                    interaction
                        .create_followup_message(&ctx.http, |response| {
                            response.content(format!("Could not download image"))
                        })
                        .await
                        .unwrap();
                    return
                },
            };
            let image_bytes = response.bytes().await.unwrap();

            let image_dictionary = self.image_dictionary.clone();
            let img_data = tokio::task::spawn_blocking(move || {
                let image = image::load_from_memory(&*image_bytes).unwrap()
                    .resize(250, 250, image::imageops::Triangle).to_rgb8();
                let new_image = image_of_image(&*image_dictionary, &image);
                let mut img_data = Vec::new();
                let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut img_data, 50);
                encoder.encode(
                    new_image.as_raw(),
                    new_image.width(),
                    new_image.height(),
                    ColorType::Rgb8
                ).unwrap();
                img_data
            }).await.unwrap();

            let loading_message = interaction
                .create_followup_message(&ctx.http, |response| {
                    response
                        .content("Uploading image\n░░░░░░░░░░░░░░░ 0%")
                })
                .await
                .unwrap();
            let loading_message_id = loading_message.id;

            let new_upload_progress_notify = Arc::new(tokio::sync::Notify::new());
            let upload_progress = Arc::new(AtomicU8::new(0));

            let img_url_handle = tokio::task::spawn_blocking({
                let upload_progress = upload_progress.clone();
                let new_upload_progress_notify = new_upload_progress_notify.clone();
                move || {
                    struct UploadProgress<R, C> {
                        on_progress: C,
                        last_perc: u8,
                        inner: R,
                        bytes_read: usize,
                        total: usize,
                    }
                    impl<R: Read, C> Read for UploadProgress<R, C>
                        where C: Fn(u8) -> () {
                        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                            self.inner.read(buf)
                                .map(|n| {
                                    self.bytes_read += n;
                                    let new_perc = (self.bytes_read * 100 / self.total) as u8;
                                    if self.last_perc != new_perc {
                                        (self.on_progress)(new_perc);
                                        self.last_perc = new_perc;
                                    }
                                    n
                                })
                        }
                    }

                    use reqwest::blocking as rq;
                    let response = rq::Client::new().post("https://litterbox.catbox.moe/resources/internals/api.php")
                        .multipart(rq::multipart::Form::new()
                            .text("reqtype", "fileupload")
                            .text("time", "72h")
                            .part("fileToUpload", rq::multipart::Part::reader(UploadProgress {
                                on_progress: move |u| {
                                    upload_progress.store(u, Ordering::Relaxed);
                                    new_upload_progress_notify.notify_waiters();
                                },
                                last_perc: 0,
                                total: img_data.len(),
                                inner: Cursor::new(img_data),
                                bytes_read: 0,
                            }).file_name("new_image.jpg"))
                        )
                        .timeout(Duration::from_secs(120)).send().unwrap();
                    response.text().unwrap()
                }
            });
            loop {
                tokio::select! {
                    _ = new_upload_progress_notify.notified() => (),
                    _ = tokio::time::sleep(Duration::from_secs(1)) => ()
                }

                let upload_progress = upload_progress.load(Ordering::Relaxed);
                let progress_bar_length = 15;
                let mut progress_bar = String::new();
                for _ in 0..(progress_bar_length as f32 * upload_progress as f32 / 100.) as u8 {
                    progress_bar += "█";
                }
                while progress_bar.chars().count() < progress_bar_length {
                    progress_bar += "░";
                }
                interaction
                    .edit_followup_message(&ctx.http, loading_message_id, |response| {
                        response
                            .content(format!("Uploading image\n{} {}%", progress_bar, upload_progress))
                    })
                    .await
                    .unwrap();
                if upload_progress == 100 {
                    break;
                }
            }
            let img_url = img_url_handle.await.unwrap();

            interaction
                .edit_followup_message(&ctx.http, loading_message.id, |response| {
                    response
                        .content("Here is you image !")
                        .create_embed(|a| {
                            a
                                .title(&img_url)
                                .image(&img_url)
                        })
                })
                .await
                .unwrap();
        }
    }
}

#[tokio::main]
async fn main() {
    // Configure the client with your Discord bot token in the environment.
    let token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    // The Application Id is usually the Bot User Id.
    let application_id: u64 =
        env::var("APPLICATION_ID").expect("Expected an application id in the environment").parse().expect("application id is not a valid id");

    println!("Loading image dictionary...");
    let image_dictionary = Arc::new({
        use rayon::prelude::*;

        let reader = ImageDictionaryReader::open(&env::var("DICTIONARY_PATH").expect("No dictionary path selected"), (32, 32)).unwrap();
        println!("Loading {} images", reader.len());
        let mut chunks = reader.split(reader.unprocessed_len() / rayon::current_num_threads());

        chunks.par_iter_mut().for_each(|c| {
            while c.process_image().unwrap_or(true) {}
        });

        reader.build_split(chunks)
    });
    println!("Library successfully loaded");

    // Build our client.
    let mut client = Client::builder(token)
        .event_handler(Handler {
            image_dictionary
        })
        .application_id(application_id)
        .await
        .expect("Error creating client");

    // Finally, start a single shard, and start listening to events.
    //
    // Shards will automatically attempt to reconnect, and will perform
    // exponential backoff until it reconnects.
    if let Err(why) = client.start().await {
        println!("Client error: {:?}", why);
    }
}
