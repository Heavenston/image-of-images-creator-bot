use std::env;
use serenity::{
    async_trait,
    client::bridge::gateway::GatewayIntents,
    model::{gateway::Ready, interactions::{Interaction, InteractionResponseType, ApplicationCommand}},
    prelude::*,
};
use serenity::model::prelude::*;
use serenity::builder::CreateEmbed;
use std::time::Duration;
use image_of_images_creator::*;
use std::sync::Arc;
use image::ColorType;

struct Handler {
    image_dictionary: Arc<ImageDictionary>,
}

#[async_trait]
impl EventHandler for Handler {
    async fn cache_ready(&self, ctx: Context, guilds: Vec<GuildId>) {
        for guild in guilds {
            println!("Registering commands for guild {:?}", guild.name(&ctx.cache).await);
            for c in guild.get_application_commands(&ctx.http).await.unwrap() {
                println!("Unregistering command {}", c.name);
                guild.delete_application_command(&ctx.http, c.id).await.unwrap();
            }
            println!("Registering command transform");
            guild.create_application_command(&ctx.http, |a| {
                a
                    .name("transform")
                    .description("Transform an image/your profile picture")
            }).await.unwrap();
        }
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
            let avatar_url = member.user.avatar_url().unwrap_or(default_avatar_url).replace("webp", "png");

            interaction
                .create_interaction_response(&ctx.http, move |response| {
                    response
                        .kind(InteractionResponseType::DeferredChannelMessageWithSource)
                })
                .await
                .unwrap();

            let response = match reqwest::get(&avatar_url).await {
                Ok(r) => r,
                Err(e) => {
                    interaction
                        .create_followup_message(&ctx.http, |response| {
                            response.content(format!("Could not download your avatar {}", e.to_string()))
                        })
                        .await
                        .unwrap();
                    return
                },
            };
            let image_bytes = response.bytes().await.unwrap();

            let image_dictionary = self.image_dictionary.clone();
            let upload_response = tokio::task::spawn_blocking(move || {
                let image = image::load_from_memory(&*image_bytes).unwrap()
                    .resize_to_fill(100, 100, image::imageops::Triangle).to_rgb8();
                let new_image = image_of_image(&*image_dictionary, &image);
                let mut img_data = Vec::new();
                let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut img_data);
                encoder.encode(
                    new_image.as_raw(),
                    new_image.width(),
                    new_image.height(),
                    ColorType::Rgb8
                ).unwrap();

                #[derive(Debug, serde::Serialize, serde::Deserialize)]
                struct ImgurUploadResponseData {
                    link: Option<String>,
                }
                #[derive(Debug, serde::Serialize, serde::Deserialize)]
                struct ImgurUploadResponse {
                    status: u16,
                    success: bool,
                    data: Option<ImgurUploadResponseData>,
                }
                use reqwest::blocking as rq;

                let form = rq::multipart::Form::new()
                    .part("image", rq::multipart::Part::bytes(img_data)
                        .file_name("hi.jpg")
                        .mime_str("image/jpg").unwrap());
                let response = rq::Client::new().post("https://api.imgur.com/3/upload")
                    .header("Authorization", "Client-ID fa0755936d63104")
                    .multipart(form).send().unwrap();

                response.json::<ImgurUploadResponse>().unwrap()
            }).await.unwrap();


            if !upload_response.success {
                interaction
                    .create_followup_message(&ctx.http, |response| {
                        response.content("Could not upload image result")
                    })
                    .await
                    .unwrap();
            }
            else {
                interaction
                    .create_followup_message(&ctx.http, |response| {
                        response
                            .create_embed(|e| e.image(upload_response.data.unwrap().link.unwrap()))
                    })
                    .await
                    .unwrap();
            }
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

        let reader = ImageDictionaryReader::open(&env::var("DICTIONARY_PATH").expect("No dictionary path selected"), (16, 16)).unwrap();
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
