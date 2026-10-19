use crate::imap::transport::{ResponseData, connection};

pub trait ResponseHandler {
    type Error: From<connection::Error>;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>>;

    fn handle_or_log(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<(), Self::Error>> {
        async {
            if let Some(response) = self.handle(response).await? {
                log::trace!("unhandled untagged response {:?}", response.parsed());
            }

            Ok(())
        }
    }
}

pub struct Unhandled;

impl ResponseHandler for Unhandled {
    type Error = connection::Error;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>> {
        std::future::ready(Ok(Some(response)))
    }
}

pub struct Chain<A, B>(pub A, pub B);

impl<A: ResponseHandler<Error = B::Error>, B: ResponseHandler> ResponseHandler for Chain<A, B> {
    type Error = B::Error;

    async fn handle(
        &mut self,
        response: ResponseData,
    ) -> Result<Option<ResponseData>, Self::Error> {
        if let Some(response) = self.0.handle(response).await? {
            self.1.handle(response).await
        } else {
            Ok(None)
        }
    }
}

impl<H: ResponseHandler> ResponseHandler for &mut H {
    type Error = H::Error;

    fn handle(
        &mut self,
        response: ResponseData,
    ) -> impl Future<Output = Result<Option<ResponseData>, Self::Error>> {
        (**self).handle(response)
    }
}
