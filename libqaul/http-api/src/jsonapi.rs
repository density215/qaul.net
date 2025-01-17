use crate::JSONAPI_MIME;
use iron::{
    BeforeMiddleware,
    prelude::*,
    headers::{ContentType, Accept, QualityItem},
    typemap,
    modifiers::Header,
    mime::{Mime, TopLevel, SubLevel},
    error::IronError,
    status::Status,
};
use json_api::{
    Document,
    Error,
    Links,
    Link,
    OptionalVec
};
use std::{
    error,
    fmt,
    io::{self, Read},
};
use serde_json;

#[derive(Debug)]
enum JsonApiError {
    MediaTypeParameters,
    NoAcceptableType,
    SerdeError(serde_json::Error),
    IoError(io::Error),
    NoDocument,
}

impl JsonApiError {
    fn reason(&self) -> String {
        match self {
            JsonApiError::MediaTypeParameters => "Content type had media type parameters in violation of https://jsonapi.org/format/#content-negotiation-servers".into(),
            JsonApiError::NoAcceptableType => "Accept header had JSON:API media type but all instances included parameters in violation of https://jsonapi.org/format/#content-negotiation-servers".into(),
            JsonApiError::SerdeError(e) => format!("Error deserializing document ({})", e),
            JsonApiError::IoError(e) => format!("IO Error while parsing body ({})", e),
            JsonApiError::NoDocument => "No document found, probably due to the content type specifiying there isn't one".into(),
        }
    }
}

impl fmt::Display for JsonApiError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Json Api Error: {}", self.reason())
    }
}

impl From<JsonApiError> for IronError {
    fn from(e: JsonApiError) -> Self {
        let about_link = match e {
            JsonApiError::MediaTypeParameters => Some(
                "https://jsonapi.org/format/#content-negotiation-servers"),
            JsonApiError::NoAcceptableType => Some(
                "https://jsonapi.org/format/#content-negotiation-servers"),
            JsonApiError::NoDocument => Some(
                "https://jsonapi.org/format/#content-negotiation-clients"),
            _ => None,
        };
        let links = if let Some(url) = about_link {
            let mut links = Links::new();
            links.insert("about".into(), Link::Url(url.into()));
            Some(links)
        } else { None };

        let status = match e {
            JsonApiError::MediaTypeParameters => Status::UnsupportedMediaType,
            JsonApiError::NoAcceptableType => Status::NotAcceptable,
            JsonApiError::SerdeError(_) => Status::BadRequest,
            JsonApiError::IoError(_) => Status::InternalServerError,
            JsonApiError::NoDocument => Status::BadRequest,
        };

        let title = match e {
            JsonApiError::MediaTypeParameters => Some("Invalid Media Type Parameters".into()),
            JsonApiError::NoAcceptableType => Some("No Acceptable Type".into()),
            JsonApiError::SerdeError(_) => Some("Deserialization Error".into()),
            JsonApiError::IoError(_) => None,
            JsonApiError::NoDocument => Some("No Document".into()),
        };

        let detail = match e {
            JsonApiError::IoError(_) => None,
            JsonApiError::NoDocument => Some("The content type indicates this is not a JSON:API request and this endpoint only supports JSON:API requests.".into()),
            _ => Some(e.reason()),
        };

        let doc = Document {
            errors: Some(vec![Error{
                links: links,
                status: Some(format!("{}", status.to_u16())),
                title,
                detail,
                ..Default::default()
            }]),
            ..Default::default()
        };

        Self::new(e, (
            serde_json::to_string(&doc).unwrap(), 
            status, 
            Header(ContentType(JSONAPI_MIME.clone()))))
    }
}

impl error::Error for JsonApiError {}

/// Use this key to get the request's `Document`
///
/// Will only decode documents when the `Content-Type` is 
/// `application/vnd.api+json`. Also checks the headers as required by the 
/// [JSON:API docs](https://jsonapi.org/format/#content-negotiation-clients).
/// If the `Content-Type` header indicates that a JSON:API document is present
/// and any of the header checks fail or the document fails to parse, an error
/// will be returned to the client and processing of the message will be aborted.
///
///
/// ```
/// fn handler(req: &mut Request) -> IronResult<Response> {
///     // Some(Document) if there was a document in the request
///     // None otherwise
///     let document = req.extensions.get::<JsonApi>();
/// }
/// ```
pub struct JsonApi;

impl typemap::Key for JsonApi { type Value = Document; } 

impl BeforeMiddleware for JsonApi {
    fn before(&self, req: &mut Request) -> IronResult<()> {
        let target_sublevel = SubLevel::Ext("vnd.api+json".into());

        // this block does two things:
        // firstly we skip any requests that don't have the Content-Type: application/vnd.api+json
        // header as they are not JSON:API requests
        // secondly we error on any requests that contain media type parameters as required by the
        // spec
        match req.headers.get::<ContentType>() {
            Some(ContentType(Mime(TopLevel::Application, 
                                  sublevel, 
                                  params))) 
            if *sublevel == target_sublevel => {
                if params.len() > 0 {
                    return Err(JsonApiError::MediaTypeParameters.into());
                }
            },
            _ => {return Ok(());},
        }

        // next up, we check the accept header
        // we hae to error if it contains the JSON:API media type and all instanced of that media
        // type are modified with media type parameters
        if let Some(Accept(v)) = req.headers.get::<Accept>() {
            let mut json_api_type = false;
            let mut with_no_params = false;
            for mime in v {
                match mime {
                    QualityItem{ 
                            item: Mime(TopLevel::Application, target_sublevel, params), 
                            quality: _ } => {
                        json_api_type = true;
                        if params.len() == 0 { 
                            with_no_params = true; 
                            break;
                        }
                    },
                    _ => {},
                }
            }

            if json_api_type && !with_no_params {
                return Err(JsonApiError::NoAcceptableType.into());
            }
        }

        // due to ownership requirements we read the body to an intermediate buffer
        // if this fails i'm honestly not sure it's recoverable but we'll return 500 INTERNAL
        // SERVER ERROR
        let mut buff = Vec::new();
        if let Err(e) = req.body.read_to_end(&mut buff) {
            return Err(JsonApiError::IoError(e).into());
        }

        // now we try to parse the body to see if it contains a valid JSON:API request
        // if it doesn't we'll return 400 BAD REQUEST
        let doc : Document = match serde_json::from_slice(&buff) {
            Ok(d) => d, 
            Err(e) => { return Err(JsonApiError::SerdeError(e).into()); } 
        };

        // after all that we put the document into the extensions map for some handler futher down
        // the chain to deal with
        req.extensions.insert::<Self>(doc);

        Ok(())
    }
}

/// Gaurds endpoints that only accept JSON:API requests
///
/// If a request passes through this middleware that does not
/// contain a valid JSON:API document, the request will be aborted and
/// an error will be returned to the client
pub struct JsonApiGaurd;
impl BeforeMiddleware for JsonApiGaurd {
    fn before(&self, req: &mut Request) -> IronResult<()> {
        match req.extensions.get::<JsonApi>() {
            Some(_) => Ok(()),
            None => Err(JsonApiError::NoDocument.into()),
        }

    }
}
