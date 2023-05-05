use jmap_proto::{
    error::method::MethodError,
    method::parse::{ParseEmailRequest, ParseEmailResponse},
    object::Object,
    types::{property::Property, value::Value},
};
use mail_parser::{
    decoders::html::html_to_text, parsers::preview::preview_text, Message, PartType,
};
use utils::map::vec_map::VecMap;

use crate::JMAP;

use super::{
    body::{ToBodyPart, TruncateBody},
    headers::HeaderToValue,
    index::PREVIEW_LENGTH,
};

impl JMAP {
    pub async fn email_parse(
        &self,
        request: ParseEmailRequest,
    ) -> Result<ParseEmailResponse, MethodError> {
        if request.blob_ids.len() > self.config.mail_parse_max_items {
            return Err(MethodError::RequestTooLarge);
        }
        let account_id = request.account_id.document_id();
        let properties = request.properties.unwrap_or_else(|| {
            vec![
                Property::BlobId,
                Property::Size,
                Property::ReceivedAt,
                Property::MessageId,
                Property::InReplyTo,
                Property::References,
                Property::Sender,
                Property::From,
                Property::To,
                Property::Cc,
                Property::Bcc,
                Property::ReplyTo,
                Property::Subject,
                Property::SentAt,
                Property::HasAttachment,
                Property::Preview,
                Property::BodyValues,
                Property::TextBody,
                Property::HtmlBody,
                Property::Attachments,
            ]
        });
        let body_properties = request.body_properties.unwrap_or_else(|| {
            vec![
                Property::PartId,
                Property::BlobId,
                Property::Size,
                Property::Name,
                Property::Type,
                Property::Charset,
                Property::Disposition,
                Property::Cid,
                Property::Language,
                Property::Location,
            ]
        });
        let fetch_text_body_values = request.fetch_text_body_values.unwrap_or(false);
        let fetch_html_body_values = request.fetch_html_body_values.unwrap_or(false);
        let fetch_all_body_values = request.fetch_all_body_values.unwrap_or(false);
        let max_body_value_bytes = request.max_body_value_bytes.unwrap_or(0);

        let mut response = ParseEmailResponse {
            account_id: request.account_id,
            parsed: VecMap::with_capacity(request.blob_ids.len()),
            not_parsable: vec![],
            not_found: vec![],
        };

        for blob_id in request.blob_ids {
            // Fetch raw message to parse
            let raw_message = match self.blob_download(&blob_id, account_id).await {
                Ok(Some(raw_message)) => raw_message,
                Ok(None) => {
                    response.not_found.push(blob_id);
                    continue;
                }
                Err(err) => {
                    tracing::error!(event = "error",
                    context = "store",
                    account_id = account_id,
                    blob_id = ?blob_id,
                    error = ?err,
                    "Failed to retrieve blob");
                    return Err(MethodError::ServerPartialFail);
                }
            };
            let message = if let Some(message) = Message::parse(&raw_message) {
                message
            } else {
                response.not_parsable.push(blob_id);
                continue;
            };

            // Prepare response
            let mut email = Object::with_capacity(properties.len());
            for property in &properties {
                match property {
                    Property::BlobId => {
                        email.append(Property::BlobId, blob_id.clone());
                    }

                    Property::Size => {
                        email.append(Property::Size, Value::UnsignedInt(raw_message.len() as u64));
                    }
                    Property::HasAttachment => {
                        email.append(
                            Property::HasAttachment,
                            Value::Bool(message.parts.iter().enumerate().any(|(part_id, part)| {
                                match &part.body {
                                    PartType::Html(_) | PartType::Text(_) => {
                                        !message.text_body.contains(&part_id)
                                            && !message.html_body.contains(&part_id)
                                    }
                                    PartType::Binary(_) | PartType::Message(_) => true,
                                    _ => false,
                                }
                            })),
                        );
                    }
                    Property::Preview => {
                        email.append(
                            Property::Preview,
                            match message
                                .text_body
                                .first()
                                .or_else(|| message.html_body.first())
                                .and_then(|idx| message.parts.get(*idx))
                                .map(|part| &part.body)
                            {
                                Some(PartType::Text(text)) => {
                                    preview_text(text.replace('\r', "").into(), PREVIEW_LENGTH)
                                        .into()
                                }
                                Some(PartType::Html(html)) => preview_text(
                                    html_to_text(html).replace('\r', "").into(),
                                    PREVIEW_LENGTH,
                                )
                                .into(),
                                _ => Value::Null,
                            },
                        );
                    }
                    Property::MessageId
                    | Property::InReplyTo
                    | Property::References
                    | Property::Sender
                    | Property::From
                    | Property::To
                    | Property::Cc
                    | Property::Bcc
                    | Property::ReplyTo
                    | Property::Subject
                    | Property::SentAt
                    | Property::Header(_) => {
                        email.append(
                            property.clone(),
                            message.parts[0].header_to_value(property, &raw_message),
                        );
                    }
                    Property::Headers => {
                        email.append(
                            Property::Headers,
                            message.parts[0].headers_to_value(&raw_message),
                        );
                    }
                    Property::TextBody | Property::HtmlBody | Property::Attachments => {
                        let list = match property {
                            Property::TextBody => &message.text_body,
                            Property::HtmlBody => &message.html_body,
                            Property::Attachments => &message.attachments,
                            _ => unreachable!(),
                        }
                        .iter();
                        email.append(
                            property.clone(),
                            list.map(|part_id| {
                                message.parts.to_body_part(
                                    *part_id,
                                    &body_properties,
                                    &raw_message,
                                    &blob_id,
                                )
                            })
                            .collect::<Vec<_>>(),
                        );
                    }
                    Property::BodyStructure => {
                        email.append(
                            Property::BodyStructure,
                            message
                                .parts
                                .to_body_part(0, &body_properties, &raw_message, &blob_id),
                        );
                    }
                    Property::BodyValues => {
                        let mut body_values = Object::with_capacity(message.parts.len());
                        for (part_id, part) in message.parts.iter().enumerate() {
                            if ((message.html_body.contains(&part_id)
                                && (fetch_all_body_values || fetch_html_body_values))
                                || (message.text_body.contains(&part_id)
                                    && (fetch_all_body_values || fetch_text_body_values)))
                                && part.is_text()
                            {
                                let (is_truncated, value) =
                                    part.body.truncate(max_body_value_bytes);
                                body_values.append(
                                    Property::_T(part_id.to_string()),
                                    Object::with_capacity(3)
                                        .with_property(
                                            Property::IsEncodingProblem,
                                            part.is_encoding_problem,
                                        )
                                        .with_property(Property::IsTruncated, is_truncated)
                                        .with_property(Property::Value, value),
                                );
                            }
                        }
                        email.append(Property::BodyValues, body_values);
                    }
                    Property::Id
                    | Property::ThreadId
                    | Property::Keywords
                    | Property::MailboxIds
                    | Property::ReceivedAt => {
                        email.append(property.clone(), Value::Null);
                    }

                    _ => {
                        return Err(MethodError::InvalidArguments(format!(
                            "Invalid property {property:?}"
                        )));
                    }
                }
            }
            response.parsed.append(blob_id, email);
        }

        Ok(response)
    }
}