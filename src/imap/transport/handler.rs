use crate::imap::transport::ResponseData;

pub trait ResponseHandler {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>>;
}

pub struct Unhandled;

impl ResponseHandler for Unhandled {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        std::future::ready(Some(response))
    }
}

pub struct Chain<A, B>(pub A, pub B);

impl<A: ResponseHandler, B: ResponseHandler> ResponseHandler for Chain<A, B> {
    async fn handle(&mut self, response: ResponseData) -> Option<ResponseData> {
        if let Some(response) = self.0.handle(response).await {
            self.1.handle(response).await
        } else {
            None
        }
    }
}

impl<H: ResponseHandler> ResponseHandler for &mut H {
    fn handle(&mut self, response: ResponseData) -> impl Future<Output = Option<ResponseData>> {
        (**self).handle(response)
    }
}
